//! One pass of the pull request poll, ported from the tracker's
//! `.github/poll/poll.cjs` (cgwalters-forge/agent-board#40): read the activity
//! of the operator and the bot in the watched repositories, decide one step for
//! each open pull request of the bot in the followed ones, and say when the
//! next pass is due. This release only previews: it takes no step.
//!
//! GET requests go through one response cache, as the review app's does
//! (cgwalters-forge/review `src/github/api.ts`): each is revalidated with
//! `If-None-Match`, or `If-Modified-Since` where GitHub sent no ETag, and a 304
//! (which costs no rate budget) reuses the stored body. The cache lives in a
//! state file the caller keeps between passes; losing it costs requests, not
//! correctness. GraphQL cannot be revalidated, so each pull request followed
//! costs one query a pass.
use crate::github::{API_ROOT, Transport, is_github_login};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub const POLL_STATE_SCHEMA: &str = "agent-board/poll-state/v1";
// Every GET and GraphQL query of one pass, 304s included. Exhaustion fails the
// rest of the pass (and so shortens the next interval), never the decisions
// already made.
pub const POLL_REQUEST_LIMIT: usize = 500;
pub const POLL_REPOSITORY_LIMIT: usize = 50;
/// The gap after a pass that saw anything change: poll.yml's wait today.
pub const MIN_INTERVAL_SECONDS: i64 = 5 * 60;
/// The gap the idle backoff stops doubling at: four passes a day.
pub const MAX_INTERVAL_SECONDS: i64 = 6 * 60 * 60;
/// The events API serves at most this many a page; the pass reads one page.
const EVENTS_PAGE: usize = 100;

// The protocol poll.cjs shares with dispatch.yml and fix.toml in the tracker.
const MARKER: &str = "tracker-poll";
const LABEL: &str = "needs-human";
const FIX_BRANCH_PREFIX: &str = "dispatch/";
const MAX_FIX_ROUNDS: u64 = 2;
/// A finished fix run's push may not be visible yet.
const FIX_RUN_GRACE_SECONDS: i64 = 5 * 60;
/// Longer than a dispatch run may take, for a fix whose run id is unknown.
const FIX_RUN_TIMEOUT_SECONDS: i64 = 7 * 60 * 60;
/// agentic-job's review status; the review it submits is what is read.
const REVIEW_CONTEXT: &str = "review";
/// The connections of `PULL_QUERY` the decision reads as whole. One that is not
/// known to be complete (more than the query asks for, or no `pageInfo`)
/// leaves the pull request waiting: a label, a change request, a failed check
/// or a placeholder subject past the cut would otherwise go unseen.
const CONNECTIONS: &[(&str, &str)] = &[
    ("/labels/pageInfo/hasNextPage", "labels"),
    ("/reviews/pageInfo/hasPreviousPage", "reviews"),
    ("/series/pageInfo/hasNextPage", "commits"),
    (
        "/commits/nodes/0/commit/statusCheckRollup/contexts/pageInfo/hasNextPage",
        "checks",
    ),
];
/// First line of the COMMENT review agentic-job leaves in place of an
/// approval it withholds (workflow/review.cjs): a human approves that head.
const WITHHELD: &str = "Approval withheld: this pull request changes the review's own gate and needs a human approval.";
const FAILED: &[&str] = &[
    "FAILURE",
    "TIMED_OUT",
    "CANCELLED",
    "ACTION_REQUIRED",
    "STARTUP_FAILURE",
    "ERROR",
];
const PASSED: &[&str] = &["SUCCESS", "SKIPPED", "NEUTRAL"];
const CONFLICTING: &str = "CONFLICTING";
// The harness's words where an author's should be (cgwalters-forge/tracker#517).
const PLACEHOLDER_PREFIX: &str = "Agent run ";
const PARTIAL_TITLE_PREFIX: &str = "[partial:";
const PARTIAL_BODY_PREFIX: &str = "**Partial work.**";
/// The events that are activity on an issue, a pull request or a branch.
const ACTIVITY_EVENTS: &[&str] = &[
    "IssueCommentEvent",
    "IssuesEvent",
    "PullRequestEvent",
    "PullRequestReviewEvent",
    "PullRequestReviewCommentEvent",
    "PushEvent",
];

const PULL_QUERY: &str = r#"query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      id number url title body isDraft mergeable headRefOid headRefName baseRefName isCrossRepository
      author { login }
      headRepositoryOwner { login }
      labels(first: 50) { nodes { name } pageInfo { hasNextPage } }
      autoMergeRequest { enabledAt }
      mergeQueueEntry { state }
      reviews(last: 100) { nodes { state body submittedAt author { __typename login } commit { oid } } pageInfo { hasPreviousPage } }
      series: commits(first: 100) { nodes { commit { oid messageHeadline } } pageInfo { hasNextPage } }
      commits(last: 1) { nodes { commit { oid statusCheckRollup { contexts(first: 100) { nodes {
        __typename
        ... on CheckRun { name status conclusion detailsUrl checkSuite { app { databaseId } } isRequired(pullRequestNumber: $number) }
        ... on StatusContext { context state targetUrl description isRequired(pullRequestNumber: $number) }
      } pageInfo { hasNextPage } } } } } }
    }
  }
}"#;

/// Who the pass reads and where fix runs are recorded.
pub struct Config {
    /// Whose activity is direction, and whose review of a head counts.
    pub operator: String,
    /// Whose pull requests are followed, and whose activity is state.
    pub bot: String,
    /// Where the fix runs that markers name run.
    pub dispatch_repo: String,
}

/// `OWNER/REPO` as it may be put into an API path: anything else could point
/// a token-bearing request at another endpoint.
pub fn validate_repository(repo: &str) -> Result<()> {
    let valid = repo.split_once('/').is_some_and(|(owner, name)| {
        is_github_login(owner)
            && (1..=100).contains(&name.len())
            && name != "."
            && name != ".."
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    });
    ensure!(valid, "invalid repository {repo:?}: expected OWNER/REPO");
    Ok(())
}

pub struct RestResponse {
    pub status: u16,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// The raw `Link` header. A 304 may leave it out, which says nothing
    /// about the pages, so absent and present without `rel="next"` differ.
    pub link: Option<String>,
    pub body: Value,
}

/// Conditional GETs against the REST API. `validator` is a header name and value.
pub trait Rest {
    fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse>;
}

/// The `rel="next"` URL of a `Link` header.
pub fn next_link(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let (target, params) = part.trim().split_once(';')?;
        params
            .split(';')
            .any(|p| p.trim() == r#"rel="next""#)
            .then(|| target.trim().strip_prefix('<')?.strip_suffix('>'))
            .flatten()
            .map(str::to_owned)
    })
}

/// What a pass keeps for the next one.
#[derive(Debug, Default, PartialEq)]
pub struct State {
    pub interval_seconds: i64,
    /// Responses by URL: `etag`, `last_modified`, `next` and `body`.
    pub responses: BTreeMap<String, Value>,
}

impl State {
    pub fn from_json(value: &Value) -> Result<Self> {
        let object = value.as_object().context("poll state is not an object")?;
        ensure!(
            object.get("schema").and_then(Value::as_str) == Some(POLL_STATE_SCHEMA),
            "unsupported poll state schema (expected {POLL_STATE_SCHEMA}); the file is a cache and may be deleted"
        );
        for key in object.keys() {
            ensure!(
                ["schema", "interval_seconds", "responses"].contains(&key.as_str()),
                "unknown poll state field {key}"
            );
        }
        let interval_seconds = object
            .get("interval_seconds")
            .and_then(Value::as_i64)
            .filter(|s| (MIN_INTERVAL_SECONDS..=MAX_INTERVAL_SECONDS).contains(s))
            .context("poll state interval_seconds is missing or out of bounds")?;
        let responses = object
            .get("responses")
            .and_then(Value::as_object)
            .context("poll state responses is not an object")?
            .iter()
            .map(|(url, entry)| {
                ensure!(url.starts_with(API_ROOT), "poll state URL outside the API");
                ensure!(
                    entry.get("body").is_some(),
                    "poll state response has no body"
                );
                Ok((url.clone(), entry.clone()))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            interval_seconds,
            responses,
        })
    }

    pub fn to_json(&self) -> Value {
        json!({
            "schema": POLL_STATE_SCHEMA,
            "interval_seconds": self.interval_seconds,
            "responses": self.responses,
        })
    }
}

/// The gap to the next pass: short while anything changed, doubling while
/// nothing does, up to four passes a day.
pub fn next_interval(previous: i64, active: bool) -> i64 {
    if active {
        MIN_INTERVAL_SECONDS
    } else {
        previous
            .clamp(MIN_INTERVAL_SECONDS / 2, MAX_INTERVAL_SECONDS)
            .saturating_mul(2)
            .min(MAX_INTERVAL_SECONDS)
    }
}

fn text<'a>(value: &'a Value, pointer: &str) -> &'a str {
    value.pointer(pointer).and_then(Value::as_str).unwrap_or("")
}

fn nodes<'a>(value: &'a Value, pointer: &str) -> &'a [Value] {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

fn timestamp(value: Option<&str>) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value?, &Rfc3339).ok()
}

/// Whether `then` is more than `seconds` before `now`; never for no time.
fn older_than(now: OffsetDateTime, then: Option<&str>, seconds: i64) -> bool {
    timestamp(then).is_some_and(|then| (now - then).whole_seconds() > seconds)
}

/// A base branch's required checks: its rulesets' (`GET rules/branches/{b}`)
/// and its classic protection's (`GET branches/{b}`, whose `protection` is
/// readable without admin rights). Either may require a check that has never
/// reported, so neither is enough alone. A protected branch whose classic
/// requirements cannot be read is an error, not "nothing required".
pub fn required_checks(rules: &Value, branch: &Value) -> Result<Vec<Value>> {
    let mut required: Vec<Value> = rules
        .as_array()
        .context("rules are not an array")?
        .iter()
        .filter(|r| r["type"] == "required_status_checks")
        .flat_map(|r| nodes(r, "/parameters/required_status_checks"))
        .cloned()
        .collect();
    match branch["protected"].as_bool() {
        Some(false) => return Ok(required),
        Some(true) => {}
        None => bail!("branch protection is unknown"),
    }
    let protection = &branch["protection"];
    if protection["enabled"] == false {
        return Ok(required);
    }
    let classic = &protection["required_status_checks"];
    ensure!(
        protection["enabled"] == true && classic.is_object(),
        "branch protection's required checks are unreadable"
    );
    if classic["enforcement_level"] == "off" {
        return Ok(required);
    }
    // `checks` binds a context to an app (null: any); `contexts` is the older,
    // unbound list of the same names.
    if let Some(checks) = classic["checks"].as_array() {
        required.extend(
            checks
                .iter()
                .map(|c| json!({"context": c["context"], "integration_id": c["app_id"]})),
        );
    } else {
        let contexts = classic["contexts"]
            .as_array()
            .context("branch protection's required checks are unreadable")?;
        required.extend(contexts.iter().map(|c| json!({"context": c})));
    }
    Ok(required)
}

#[derive(Debug, Default, PartialEq)]
pub struct Checks {
    /// Failed check names and their log URLs.
    pub failed: Vec<(String, Option<String>)>,
    pub pending: Vec<String>,
}

/// The checks that gate a merge. `required` is the base branch's required
/// contexts and integration IDs from [`required_checks`]; GitHub's own `isRequired` is added to it (it is
/// false on drafts, so it is not enough alone). A repository that requires
/// nothing is judged on all its checks.
pub fn classify_checks(pull: &Value, required: &[Value]) -> Checks {
    type Check<'a> = (&'a str, Option<&'a str>, bool, &'a str, Option<u64>);
    let checks: Vec<Check<'_>> = nodes(
        pull,
        "/commits/nodes/0/commit/statusCheckRollup/contexts/nodes",
    )
    .iter()
    .filter_map(|c| {
        let flag = c["isRequired"].as_bool().unwrap_or(false);
        if c["__typename"] == "CheckRun" {
            let state = if c["status"] == "COMPLETED" {
                c["conclusion"].as_str().unwrap_or("")
            } else {
                "PENDING"
            };
            Some((
                text(c, "/name"),
                c["detailsUrl"].as_str(),
                flag,
                state,
                c.pointer("/checkSuite/app/databaseId")
                    .and_then(Value::as_u64),
            ))
        } else {
            let name = text(c, "/context");
            (name != REVIEW_CONTEXT)
                .then(|| (name, c["targetUrl"].as_str(), flag, text(c, "/state"), None))
        }
    })
    .collect();
    let mut names = required.to_vec();
    let context = |r: &Value| r.as_str().unwrap_or_else(|| text(r, "/context")).to_owned();
    // StatusContext has no app provenance in this query. It cannot satisfy an
    // integration-bound rule; that requirement remains pending rather than
    // falling back to a same-name result from an unknown source.
    let matches = |r: &Value, name: &str, integration: Option<u64>| {
        context(r) == name
            && r.get("integration_id")
                .and_then(Value::as_u64)
                .is_none_or(|id| Some(id) == integration)
    };
    for (name, _, flag, _, integration) in &checks {
        if *flag && !names.iter().any(|r| matches(r, name, *integration)) {
            names.push(json!({"context": name, "integration_id": integration}));
        }
    }
    let all = names.is_empty();
    // A ruleset that requires the review status is answered by the review of
    // the head; as a check it would stay pending, never having been reported.
    names.retain(|r| context(r) != REVIEW_CONTEXT);
    let relevant = checks.iter().filter(|(name, _, _, _, integration)| {
        all || names.iter().any(|r| matches(r, name, *integration))
    });
    let mut out = Checks::default();
    for (name, url, _, state, _) in relevant {
        if FAILED.contains(state) {
            out.failed.push(((*name).into(), url.map(str::to_owned)));
        } else if !PASSED.contains(state) {
            out.pending.push((*name).into());
        }
    }
    // A required check that has not reported at all is still to come.
    for name in names {
        if !checks
            .iter()
            .any(|(n, _, _, _, integration)| matches(&name, n, *integration))
        {
            out.pending.push(context(&name));
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Review {
    Approved,
    ChangesRequested,
    Withheld,
}

/// The newest verdict on the head by the operator. A
/// review of an older head says nothing of this one.
pub fn review_at_head(pull: &Value, operator: &str) -> Option<Review> {
    let head = text(pull, "/headRefOid");
    nodes(pull, "/reviews/nodes")
        .iter()
        .filter(|r| {
            let author = text(r, "/author/login");
            // An app's slug may be a user's login too: the type tells them apart.
            let kind = text(r, "/author/__typename");
            // The shared Actions bot does not authenticate a particular workflow.
            // Fail closed until independently validated job provenance is available.
            (kind == "User" && author == operator)
                && !head.is_empty()
                && text(r, "/commit/oid") == head
        })
        .filter_map(|r| {
            let submitted = OffsetDateTime::parse(text(r, "/submittedAt"), &Rfc3339).ok()?;
            let verdict = match text(r, "/state") {
                "APPROVED" => Some(Review::Approved),
                "CHANGES_REQUESTED" => Some(Review::ChangesRequested),
                "COMMENTED" if text(r, "/body").lines().next() == Some(WITHHELD) => {
                    Some(Review::Withheld)
                }
                _ => None,
            }?;
            Some((submitted, verdict))
        })
        .max_by_key(|(submitted, _)| *submitted)
        .map(|(_, verdict)| verdict)
}

/// The poll's round count, from its marker comment on the pull request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Marker {
    pub id: Option<u64>,
    pub rounds: u64,
    pub sha: Option<String>,
    pub run: Option<u64>,
    pub at: Option<String>,
    /// Written next to the label; with the label gone, someone took it off.
    pub stopped: bool,
}

fn parse_marker(body: &str) -> Option<Value> {
    let open = format!("<!-- {MARKER}: ");
    // As poll.cjs's one `\{.*?\} -->` match: the first marker whose line holds
    // an object closed by the comment's end. If that is not JSON, the comment
    // carries no marker; a later one in it is not tried.
    let json = body.match_indices(&open).find_map(|(start, _)| {
        let rest = &body[start + open.len()..];
        let line = rest.split('\n').next().unwrap_or("");
        line.starts_with('{')
            .then(|| line.find("} -->"))
            .flatten()
            .map(|end| &line[..=end])
    })?;
    serde_json::from_str(json).ok()
}

/// The poll's own comment: the first one by `login` that carries the marker.
/// Anyone else's is ignored, so a visitor cannot reset the round count.
pub fn find_marker(comments: &[Value], login: &str) -> Marker {
    for comment in comments {
        if text(comment, "/user/login") != login {
            continue;
        }
        let Some(state) = parse_marker(text(comment, "/body")) else {
            continue;
        };
        // A safe integer, as Number.isSafeInteger in poll.cjs.
        let Some(rounds) = state["rounds"].as_u64().filter(|r| *r < 1 << 53) else {
            continue;
        };
        return Marker {
            id: comment["id"].as_u64(),
            rounds,
            sha: state["sha"].as_str().map(str::to_owned),
            run: state["run"].as_u64().filter(|r| *r > 0),
            at: state["at"].as_str().map(str::to_owned),
            stopped: state["stopped"].as_bool().unwrap_or(false),
        };
    }
    Marker::default()
}

fn labelled(pull: &Value) -> bool {
    nodes(pull, "/labels/nodes")
        .iter()
        .any(|l| l["name"] == LABEL)
}

/// What a dispatch run opened: a fix can push nowhere else, and a draft on any
/// other branch is its author's to mark ready.
pub fn is_harness(pull: &Value, bot: &str) -> bool {
    pull["isCrossRepository"].as_bool().unwrap_or(false)
        && text(pull, "/headRepositoryOwner/login") == bot
        && text(pull, "/headRefName").starts_with(FIX_BRANCH_PREFIX)
}

/// Whether the pull request is still ours to follow after this pass decided
/// `action` for it, which keeps the interval short: an open dispatch pull
/// request that is not (and was not just) handed to a human.
pub fn in_flight(pull: &Value, bot: &str, action: &str) -> bool {
    is_harness(pull, bot) && action != "needs-human" && !labelled(pull)
}

/// The connections the query did not read whole. A head with no checks at all
/// has a null rollup, which is complete.
fn truncated(pull: &Value) -> Vec<&'static str> {
    CONNECTIONS
        .iter()
        .filter(|(pointer, _)| {
            let none = pointer.starts_with("/commits/")
                && pull.pointer("/commits/nodes/0/commit/statusCheckRollup") == Some(&Value::Null);
            !none && pull.pointer(pointer).and_then(Value::as_bool) != Some(false)
        })
        .map(|(_, what)| *what)
        .collect()
}

fn is_placeholder(text: &str) -> bool {
    text.strip_prefix(PLACEHOLDER_PREFIX)
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
}

fn placeholder_commits(pull: &Value) -> bool {
    nodes(pull, "/series/nodes")
        .iter()
        .any(|n| is_placeholder(text(n, "/commit/messageHeadline")))
}

/// What is wrong with the title and description, which no fix run can change.
fn undescribed(pull: &Value) -> Vec<&'static str> {
    let title = text(pull, "/title").trim();
    let partial = title.starts_with(PARTIAL_TITLE_PREFIX)
        || text(pull, "/body")
            .trim_start()
            .starts_with(PARTIAL_BODY_PREFIX);
    let mut out = Vec::new();
    if is_placeholder(title) {
        out.push("its title is the harness placeholder");
    }
    if partial {
        out.push("it is marked as partial work");
    }
    out
}

#[derive(Debug, PartialEq)]
pub struct Decision {
    /// skip, wait, ready, fix, merge or needs-human.
    pub action: &'static str,
    pub reason: String,
    pub round: Option<u64>,
}

fn step(action: &'static str, reason: impl Into<String>) -> Decision {
    Decision {
        action,
        reason: reason.into(),
        round: None,
    }
}

/// The one step for a pull request, as poll.cjs's `decide`, except that the
/// verdict is the review of the head rather than a status and a comment.
/// `fix_run` is the marker's run when the marker names one for this head.
pub fn decide(
    config: &Config,
    pull: &Value,
    checks: &Checks,
    marker: &Marker,
    fix_run: Option<&Value>,
    now: OffsetDateTime,
) -> Decision {
    let head = text(pull, "/headRefOid");
    let base = text(pull, "/baseRefName");
    if labelled(pull) {
        return step("skip", format!("labelled {LABEL}"));
    }
    let partial = truncated(pull);
    if !partial.is_empty() {
        return step(
            "wait",
            format!(
                "more {} than one query reads; not decided on part of them",
                partial.join(", ")
            ),
        );
    }
    // `stopped` is what needs-human wrote next to the label, so with the label
    // gone someone took it off: that asks for fresh rounds, on this head too.
    // Only the bot's own marker is read, and taking a label off needs triage
    // access, so it is not a visitor's to ask for.
    let reset = marker.stopped;
    let fresh = Marker::default();
    let marker = if reset { &fresh } else { marker };
    let harness = is_harness(pull, &config.bot);
    let not_ours =
        format!("not a {FIX_BRANCH_PREFIX} branch of the bot's fork, left to its author");
    let review = review_at_head(pull, &config.operator);
    let mut failing = Vec::new();
    if review == Some(Review::ChangesRequested) {
        failing.push("review requested changes".to_owned());
    }
    failing.extend(
        checks
            .failed
            .iter()
            .map(|(name, _)| format!("check {name} failed")),
    );
    if pull["mergeable"] == CONFLICTING {
        failing.push(format!("conflicts with {base}"));
    }
    if placeholder_commits(pull) {
        failing.push("a commit subject is the harness placeholder".into());
    }
    if !failing.is_empty() {
        let why = failing.join(", ");
        if !harness {
            return step("skip", format!("{why}; {not_ours}"));
        }
        if marker.sha.as_deref() == Some(head) {
            // A fix run that is over and left the head as it was is blocked;
            // only one that succeeded gets the grace for its push to show.
            if let Some(run) = fix_run.filter(|r| r["status"] == "completed") {
                let conclusion = run["conclusion"].as_str().unwrap_or("null");
                if conclusion != "success"
                    || older_than(now, run["updated_at"].as_str(), FIX_RUN_GRACE_SECONDS)
                {
                    return step(
                        "needs-human",
                        format!(
                            "fix round {} is blocked: its run https://github.com/{}/actions/runs/{} ended ({conclusion}) and pushed no new head for {} ({why})",
                            marker.rounds,
                            config.dispatch_repo,
                            marker.run.unwrap_or_default(),
                            short(head)
                        ),
                    );
                }
            }
            if marker.run.is_none()
                && older_than(now, marker.at.as_deref(), FIX_RUN_TIMEOUT_SECONDS)
            {
                return step(
                    "needs-human",
                    format!(
                        "the fix run for {} ended without changing the head ({why})",
                        short(head)
                    ),
                );
            }
            return step(
                "wait",
                format!("fix round {} is running for {}", marker.rounds, short(head)),
            );
        }
        if marker.rounds >= MAX_FIX_ROUNDS {
            return step(
                "needs-human",
                format!(
                    "{} fix rounds used and {} still fails ({why})",
                    marker.rounds,
                    short(head)
                ),
            );
        }
        return Decision {
            action: "fix",
            reason: if reset {
                format!("{why}; {LABEL} was removed, so the rounds start again")
            } else {
                why
            },
            round: Some(marker.rounds + 1),
        };
    }
    let unwritten = undescribed(pull);
    if !unwritten.is_empty() {
        let what = unwritten.join(" and ");
        return if harness {
            step(
                "needs-human",
                format!(
                    "{what}, which a fix run cannot change; write the title and description (and see that the work is whole) before removing the label"
                ),
            )
        } else {
            step("skip", format!("{what}; {not_ours}"))
        };
    }
    if pull["isDraft"].as_bool().unwrap_or(false) {
        return if harness {
            step("ready", "draft with no failed check")
        } else {
            step("skip", format!("draft, {not_ours}"))
        };
    }
    match review {
        Some(Review::Approved) => {}
        Some(Review::Withheld) => {
            return step(
                "wait",
                "the review withheld its approval of the head; it needs a human approval",
            );
        }
        _ => return step("wait", "no approving review of the head yet"),
    }
    if !checks.pending.is_empty() {
        return step(
            "wait",
            format!("checks pending: {}", checks.pending.join(", ")),
        );
    }
    if !pull["autoMergeRequest"].is_null() || !pull["mergeQueueEntry"].is_null() {
        return step("skip", "auto-merge already on or queued");
    }
    // GitHub works mergeability out again after every push to either side; an
    // answer that is not in yet is neither a conflict nor a go-ahead.
    if pull["mergeable"] != "MERGEABLE" {
        return step("wait", format!("mergeability with {base} not known yet"));
    }
    step("merge", "review approved and checks green")
}

/// The fields of an event the pass reads; the rest is not kept in the state.
fn slim_event(event: &Value) -> Value {
    let payload = &event["payload"];
    json!({
        "id": event["id"],
        "type": event["type"],
        "actor": {"login": event["actor"]["login"]},
        "payload": {
            "action": payload["action"],
            "ref": payload["ref"],
            "number": payload["issue"]["number"].as_u64().or(payload["pull_request"]["number"].as_u64()),
            "review": payload["review"]["state"],
            "label": payload["label"]["name"],
        },
    })
}

fn events_url(repo: &str) -> String {
    format!("{API_ROOT}repos/{repo}/events?per_page={EVENTS_PAGE}")
}

fn event_id(event: &Value) -> Option<u64> {
    event["id"].as_str()?.parse().ok()
}

/// One activity line, or none for an event that is not activity.
fn describe(repo: &str, event: &Value, config: &Config) -> Option<String> {
    let login = text(event, "/actor/login");
    let role = if login == config.operator {
        "direction"
    } else if login == config.bot {
        "state"
    } else {
        return None;
    };
    let payload = &event["payload"];
    let kind = text(event, "/type");
    let what = match kind {
        "IssueCommentEvent" => "commented".to_owned(),
        "PullRequestReviewCommentEvent" => "commented on the diff".to_owned(),
        "PullRequestReviewEvent" => format!(
            "reviewed ({})",
            payload["review"]
                .as_str()
                .unwrap_or("unknown")
                .to_lowercase()
        ),
        "IssuesEvent" | "PullRequestEvent" => match payload["label"].as_str() {
            Some(label) => format!("{} {label}", text(payload, "/action")),
            None => text(payload, "/action").to_owned(),
        },
        "PushEvent" => {
            return Some(format!(
                "{repo} activity: {login} pushed {} ({role})",
                text(payload, "/ref")
            ));
        }
        _ => return None,
    };
    let number = payload["number"].as_u64()?;
    Some(format!("{repo}#{number} activity: {login} {what} ({role})"))
}

/// The result of one pass.
pub struct Outcome {
    pub lines: Vec<String>,
    pub errors: usize,
    pub activity: usize,
    pub in_flight: usize,
    /// The state to keep for the next pass; its interval is the next gap.
    pub state: State,
}

struct Pass<'a, R, G> {
    rest: &'a R,
    graphql: &'a G,
    config: &'a Config,
    now: OffsetDateTime,
    previous: BTreeMap<String, Value>,
    /// Only what this pass used is kept, which bounds the state file.
    used: BTreeMap<String, Value>,
    remaining: usize,
}

impl<R: Rest, G: Transport> Pass<'_, R, G> {
    fn spend(&mut self) -> Result<()> {
        ensure!(
            self.remaining > 0,
            "poll request budget ({POLL_REQUEST_LIMIT}) exhausted"
        );
        self.remaining -= 1;
        Ok(())
    }

    /// One conditional GET; `slim` is applied to a fresh body before it is kept.
    fn get_url(&mut self, url: &str, slim: fn(&Value) -> Value) -> Result<(Value, Option<String>)> {
        self.spend()?;
        let cached = self.used.get(url).or(self.previous.get(url)).cloned();
        let validator = cached.as_ref().and_then(|c| {
            c["etag"]
                .as_str()
                .map(|v| ("If-None-Match", v))
                .or(c["last_modified"]
                    .as_str()
                    .map(|v| ("If-Modified-Since", v)))
        });
        let response = self.rest.get(url, validator)?;
        let entry = match (response.status, cached) {
            // The ETag covers the body only: a list that grew past this page
            // answers 304 with a new `Link`. Without one, the cached pages stand.
            (304, Some(mut cached)) => {
                if let Some(link) = response.link {
                    cached["next"] = next_link(&link).into();
                }
                cached
            }
            (304, None) => bail!("GET {url}: 304 for a request that was not conditional"),
            _ => json!({
                "etag": response.etag,
                "last_modified": response.last_modified,
                "next": response.link.as_deref().and_then(next_link),
                "body": slim(&response.body),
            }),
        };
        self.used.insert(url.to_owned(), entry.clone());
        Ok((
            entry["body"].clone(),
            entry["next"].as_str().map(str::to_owned),
        ))
    }

    fn get(&mut self, path: &str) -> Result<Value> {
        Ok(self.get_url(&format!("{API_ROOT}{path}"), Value::clone)?.0)
    }

    /// Every page of a list, following `Link` within the API only.
    fn paged(&mut self, path: &str) -> Result<Vec<Value>> {
        let mut url = Some(format!("{API_ROOT}{path}"));
        let mut all = Vec::new();
        while let Some(page) = url {
            ensure!(
                page.starts_with(API_ROOT),
                "pagination left the API: {page}"
            );
            let (body, next) = self.get_url(&page, Value::clone)?;
            all.extend(
                body.as_array()
                    .context("GitHub list is not an array")?
                    .iter()
                    .cloned(),
            );
            url = next;
        }
        Ok(all)
    }

    /// The operator's and the bot's activity since the last pass, oldest first.
    /// New is what the previous pass's page did not have, rather than what is
    /// past the newest id it had: GitHub publishes some events late, under an
    /// older id.
    fn activity(&mut self, repo: &str, lines: &mut Vec<String>) -> Result<usize> {
        let url = events_url(repo);
        let seen: Option<BTreeSet<u64>> = self
            .previous
            .get(&url)
            .map(|entry| nodes(entry, "/body").iter().filter_map(event_id).collect());
        let (body, _) = self.get_url(&url, |body| {
            Value::Array(
                body.as_array()
                    .map(|e| e.iter().map(slim_event).collect())
                    .unwrap_or_default(),
            )
        })?;
        let events = body.as_array().context("GitHub events are not an array")?;
        let fresh: Vec<_> = events
            .iter()
            .filter(|e| {
                event_id(e).is_some_and(|id| seen.as_ref().is_none_or(|seen| !seen.contains(&id)))
            })
            .collect();
        if let Some(seen) = &seen
            && !seen.is_empty()
            && events.len() >= EVENTS_PAGE
            && fresh.len() == events.len()
        {
            lines.push(format!("{repo} activity: more than {EVENTS_PAGE} events since the last pass; older ones were not read"));
        }
        let mut count = 0;
        for event in fresh.iter().rev() {
            if !ACTIVITY_EVENTS.contains(&text(event, "/type")) {
                continue;
            }
            if let Some(line) = describe(repo, event, self.config) {
                lines.push(line);
                count += 1;
            }
        }
        Ok(count)
    }

    /// One pull request's decision line, and whether it is in flight.
    fn pull(
        &mut self,
        repo: &str,
        number: u64,
        required: &mut BTreeMap<String, Vec<Value>>,
    ) -> Result<(String, bool)> {
        let (owner, name) = repo.split_once('/').context("repository")?;
        self.spend()?;
        let data = self.graphql.graphql(
            PULL_QUERY,
            json!({"owner": owner, "name": name, "number": number}),
        )?;
        let pull = data
            .pointer("/repository/pullRequest")
            .filter(|p| p.is_object())
            .context("pull request not found")?
            .clone();
        let base = text(&pull, "/baseRefName").to_owned();
        if !required.contains_key(&base) {
            let encoded: String = url::form_urlencoded::byte_serialize(base.as_bytes()).collect();
            let rules = self.get(&format!("repos/{repo}/rules/branches/{encoded}"))?;
            let branch = self.get(&format!("repos/{repo}/branches/{encoded}"))?;
            required.insert(base.clone(), required_checks(&rules, &branch)?);
        }
        let checks = classify_checks(&pull, &required[&base]);
        let comments = self.paged(&format!(
            "repos/{repo}/issues/{number}/comments?per_page=100"
        ))?;
        let marker = find_marker(&comments, &self.config.bot);
        let head = text(&pull, "/headRefOid");
        let fix_run = match marker.run {
            Some(run) if marker.sha.as_deref() == Some(head) => Some(self.get(&format!(
                "repos/{}/actions/runs/{run}",
                self.config.dispatch_repo
            ))?),
            _ => None,
        };
        let decision = decide(
            self.config,
            &pull,
            &checks,
            &marker,
            fix_run.as_ref(),
            self.now,
        );
        let line = format!(
            "{repo}#{number} {} {}: {}",
            short(head),
            decision.action,
            decision.reason
        );
        Ok((line, in_flight(&pull, &self.config.bot, decision.action)))
    }

    fn follow(&mut self, repo: &str, outcome: &mut Outcome) -> Result<()> {
        let open = self.paged(&format!("repos/{repo}/pulls?state=open&per_page=100"))?;
        let mut numbers: Vec<u64> = open
            .iter()
            .filter(|p| text(p, "/user/login") == self.config.bot)
            .filter_map(|p| p["number"].as_u64())
            .collect();
        numbers.sort_unstable();
        let mut required = BTreeMap::new();
        for number in numbers {
            match self.pull(repo, number, &mut required) {
                Ok((line, flying)) => {
                    outcome.lines.push(line);
                    outcome.in_flight += usize::from(flying);
                }
                Err(error) => {
                    outcome.errors += 1;
                    outcome
                        .lines
                        .push(format!("{repo}#{number} error: {error:#}"));
                }
            }
        }
        Ok(())
    }
}

/// One pass: activity in `watch` and `follow`, then a decision for each open
/// pull request of the bot in `follow`. Takes no step on GitHub.
pub fn run<R: Rest, G: Transport>(
    rest: &R,
    graphql: &G,
    config: &Config,
    state: State,
    watch: &[String],
    follow: &[String],
    now: OffsetDateTime,
) -> Result<Outcome> {
    for login in [&config.operator, &config.bot] {
        ensure!(is_github_login(login), "invalid login {login:?}");
    }
    validate_repository(&config.dispatch_repo)?;
    let repositories: BTreeSet<&String> = watch.iter().chain(follow).collect();
    ensure!(
        !repositories.is_empty(),
        "poll needs at least one repository"
    );
    ensure!(
        repositories.len() <= POLL_REPOSITORY_LIMIT,
        "poll repository limit exceeded ({POLL_REPOSITORY_LIMIT})"
    );
    for repo in &repositories {
        validate_repository(repo)?;
    }
    let mut pass = Pass {
        rest,
        graphql,
        config,
        now,
        previous: state.responses,
        used: BTreeMap::new(),
        remaining: POLL_REQUEST_LIMIT,
    };
    let mut outcome = Outcome {
        lines: Vec::new(),
        errors: 0,
        activity: 0,
        in_flight: 0,
        state: State::default(),
    };
    for repo in &repositories {
        match pass.activity(repo, &mut outcome.lines) {
            Ok(count) => outcome.activity += count,
            Err(error) => {
                outcome.errors += 1;
                outcome.lines.push(format!("{repo} error: {error:#}"));
                // Keep the page this pass could not replace, or the next one
                // would report all of it again.
                let url = events_url(repo);
                if let Some(entry) = pass.previous.get(&url).cloned() {
                    pass.used.entry(url).or_insert(entry);
                }
            }
        }
    }
    let follow: BTreeSet<&String> = follow.iter().collect();
    for repo in follow {
        if let Err(error) = pass.follow(repo, &mut outcome) {
            outcome.errors += 1;
            outcome.lines.push(format!("{repo} error: {error:#}"));
        }
    }
    // What could not be read counts as a change: an API error must not stretch
    // the gap while a pull request may be waiting on it.
    let active = outcome.activity > 0 || outcome.in_flight > 0 || outcome.errors > 0;
    let previous = if state.interval_seconds == 0 {
        MIN_INTERVAL_SECONDS
    } else {
        state.interval_seconds
    };
    outcome.state = State {
        interval_seconds: next_interval(previous, active),
        responses: pass.used,
    };
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const BOT: &str = "cgwalters-bot";
    const HEAD: &str = "488f45ebacd03a8bc88dceb3d703a5322850ce77";

    fn config() -> Config {
        Config {
            operator: "cgwalters".into(),
            bot: BOT.into(),
            dispatch_repo: "cgwalters-forge/tracker".into(),
        }
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::parse("2026-10-12T02:00:00Z", &Rfc3339).unwrap()
    }

    // Recorded by the tracker's poll (cgwalters-forge/tracker .github/poll/testdata):
    // pull-470 is a dispatch run's pull request from the bot's fork, approved and
    // green; pull-143 a draft on a branch of the repository itself whose `ci`
    // failed; pull-511 one of a run cut off before it wrote a message.
    fn fixture(name: &str) -> Value {
        serde_json::from_str(match name {
            "pull-470" => include_str!("../../../fixtures/poll/pull-470.json"),
            "pull-143" => include_str!("../../../fixtures/poll/pull-143.json"),
            "pull-511" => include_str!("../../../fixtures/poll/pull-511.json"),
            "rules" => include_str!("../../../fixtures/poll/rules-agentic-job.json"),
            "comments-470" => include_str!("../../../fixtures/poll/comments-470.json"),
            _ => unreachable!("{name}"),
        })
        .unwrap()
    }

    fn required() -> Vec<Value> {
        required_checks(&fixture("rules"), &json!({"protected": false})).unwrap()
    }

    /// A branch whose classic protection requires `checks`.
    fn protected(checks: Value) -> Value {
        json!({"protected": true, "protection": {"enabled": true, "required_status_checks": {
            "enforcement_level": "non_admins", "checks": checks}}})
    }

    fn review(state: &str, author: Value, oid: &str, body: &str) -> Value {
        json!({"state": state, "body": body, "submittedAt": "2026-10-12T01:00:00Z", "author": author, "commit": {"oid": oid}})
    }

    /// Trusted operator author for the decision table.
    fn job() -> Value {
        user("cgwalters")
    }

    fn user(login: &str) -> Value {
        json!({"__typename": "User", "login": login})
    }

    /// Synthetic operator-verdict cases, not parity with the recorded job verdicts.
    fn pull(name: &str, change: Value) -> Value {
        pull_with_author(name, change, job())
    }

    /// A recorded pull request with fields replaced and some of its head's
    /// contexts changed (null removes one). Mergeable on main unless a case
    /// says otherwise. The recordings predate real reviews, so unless a case
    /// gives `reviews`, the recorded `review` status becomes the supplied author's
    /// review of the head. They predate `pageInfo` too: each connection is
    /// complete unless `truncated` sets its flag (null removes it).
    fn pull_with_author(name: &str, change: Value, author: Value) -> Value {
        let mut p = fixture(name);
        p["mergeable"] = "MERGEABLE".into();
        p["baseRefName"] = "main".into();
        let mut change = change.as_object().cloned().unwrap_or_default();
        let contexts = change.remove("contexts");
        let truncated = change.remove("truncated");
        let explicit = change.contains_key("reviews");
        for (key, value) in change {
            p[key] = value;
        }
        let list = p
            .pointer_mut("/commits/nodes/0/commit/statusCheckRollup/contexts/nodes")
            .and_then(Value::as_array_mut)
            .unwrap();
        for check in list.iter_mut() {
            if check["__typename"] == "CheckRun" {
                check["checkSuite"] = json!({"app": {"databaseId": 15368}});
            }
        }
        for (key, value) in contexts
            .as_ref()
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            let index = list
                .iter()
                .position(|n| n["name"] == *key || n["context"] == *key)
                .unwrap_or_else(|| panic!("fixture has no context {key}"));
            if value.is_null() {
                list.remove(index);
            } else {
                for (field, v) in value.as_object().unwrap() {
                    list[index][field] = v.clone();
                }
            }
        }
        if !explicit {
            let status = list
                .iter()
                .find(|n| n["context"] == REVIEW_CONTEXT)
                .map(|n| text(n, "/state").to_owned());
            let head = text(&p, "/headRefOid").to_owned();
            p["reviews"] = json!({"nodes": match status.as_deref() {
                Some("SUCCESS") => vec![review("APPROVED", author, &head, "VERDICT: APPROVE")],
                Some("FAILURE") => vec![review("CHANGES_REQUESTED", author, &head, "VERDICT: CHANGES")],
                _ => vec![],
            }});
        }
        if p.get("series").is_none() {
            p["series"] = json!({"nodes": []});
        }
        for (pointer, what) in CONNECTIONS {
            let (connection, flag) = pointer.rsplit_once("/pageInfo/").unwrap();
            let info = p.pointer_mut(connection).unwrap().as_object_mut().unwrap();
            match truncated.as_ref().and_then(|t| t.get(what)) {
                Some(Value::Null) => {
                    info.remove("pageInfo");
                }
                Some(value) => {
                    info.insert("pageInfo".into(), json!({flag: value}));
                }
                None => {
                    info.entry("pageInfo").or_insert(json!({flag: false}));
                }
            }
        }
        p
    }

    fn marker_body(state: &Value, text: &str) -> String {
        format!(
            "<!-- {MARKER}: {state} -->\n{text}\n\nGenerated-by: https://github.com/cgwalters/#llms"
        )
    }

    /// A marker as the bot's comment carries it.
    fn marker(state: Value) -> Marker {
        let comment =
            json!({"id": 1, "user": {"login": BOT}, "body": marker_body(&state, "Fix round.")});
        let marker = find_marker(&[comment], BOT);
        assert_eq!(marker.rounds, state["rounds"].as_u64().unwrap(), "{state}");
        marker
    }

    #[test]
    fn decide_table() {
        let failed_run = json!({"status": "completed", "conclusion": "failure", "updated_at": "2026-10-12T01:59:30Z"});
        let running = json!({"status": "in_progress", "updated_at": "2026-10-12T01:00:00Z"});
        let reworded = json!({"nodes": [{"commit": {"oid": "f".repeat(40), "messageHeadline": "check: Say why"}}]});
        let partial_title = "[partial: stopped at a limit] Wrote the first half";
        let partial_body = "**Partial work.** The run was stopped at its timeout before the task was done; this is what it handed back.\n\nrest";
        let ci_failed = json!({"ci": {"conclusion": "FAILURE"}});
        let changes = json!({"review": {"state": "FAILURE"}});
        let label = json!({"nodes": [{"name": LABEL}]});
        let someone = json!({"login": "someone"});
        let head_511 = text(&fixture("pull-511"), "/headRefOid").to_owned();
        let none = Value::Null;
        #[rustfmt::skip]
        // Name, fixture, change, marker, fix run, action and round.
        type Case<'a> = (&'a str, &'a str, Value, Value, Value, &'a str, Option<u64>);
        let cases: Vec<Case> = vec![
            (
                "approved and green merges",
                "pull-470",
                json!({}),
                json!({"rounds": 0}),
                none.clone(),
                "merge",
                None,
            ),
            (
                "approved with auto-merge on is left alone",
                "pull-470",
                json!({"autoMergeRequest": {"enabledAt": "x"}}),
                json!({"rounds": 0}),
                none.clone(),
                "skip",
                None,
            ),
            (
                "approved and queued is left alone",
                "pull-470",
                json!({"mergeQueueEntry": {"state": "QUEUED"}}),
                json!({"rounds": 0}),
                none.clone(),
                "skip",
                None,
            ),
            (
                "approved but conflicting gets a fix",
                "pull-470",
                json!({"mergeable": CONFLICTING}),
                json!({"rounds": 0}),
                none.clone(),
                "fix",
                Some(1),
            ),
            (
                "conflicting with no verdict yet gets a fix",
                "pull-470",
                json!({"mergeable": CONFLICTING, "contexts": {"review": null}}),
                json!({"rounds": 0}),
                none.clone(),
                "fix",
                Some(1),
            ),
            (
                "a conflict with a fix running for this head waits",
                "pull-470",
                json!({"mergeable": CONFLICTING}),
                json!({"rounds": 1, "sha": HEAD, "run": 7}),
                running.clone(),
                "wait",
                None,
            ),
            (
                "a conflict with the rounds exhausted goes to a human",
                "pull-470",
                json!({"mergeable": CONFLICTING}),
                json!({"rounds": 2, "sha": "older"}),
                none.clone(),
                "needs-human",
                None,
            ),
            (
                "another fork's conflicting branch gets no fix",
                "pull-470",
                json!({"mergeable": CONFLICTING, "headRepositoryOwner": someone}),
                json!({"rounds": 0}),
                none.clone(),
                "skip",
                None,
            ),
            (
                "approved with mergeability unknown waits",
                "pull-470",
                json!({"mergeable": "UNKNOWN"}),
                json!({"rounds": 0}),
                none.clone(),
                "wait",
                None,
            ),
            (
                "approved with ci running waits",
                "pull-470",
                json!({"contexts": {"ci": {"status": "IN_PROGRESS", "conclusion": null}}}),
                json!({"rounds": 0}),
                none.clone(),
                "wait",
                None,
            ),
            (
                "approved with ci not reported waits",
                "pull-470",
                json!({"contexts": {"ci": null}}),
                json!({"rounds": 0}),
                none.clone(),
                "wait",
                None,
            ),
            (
                "no review waits",
                "pull-470",
                json!({"contexts": {"review": null}}),
                json!({"rounds": 0}),
                none.clone(),
                "wait",
                None,
            ),
            (
                "pending review waits",
                "pull-470",
                json!({"contexts": {"review": {"state": "PENDING"}}}),
                json!({"rounds": 0}),
                none.clone(),
                "wait",
                None,
            ),
            (
                "a draft with ci running is marked ready",
                "pull-470",
                json!({"isDraft": true, "contexts": {"review": null, "ci": {"status": "QUEUED", "conclusion": null}}}),
                json!({"rounds": 0}),
                none.clone(),
                "ready",
                None,
            ),
            (
                "a draft with failed ci gets a fix, not ready",
                "pull-470",
                json!({"isDraft": true, "contexts": ci_failed}),
                json!({"rounds": 0}),
                none.clone(),
                "fix",
                Some(1),
            ),
            (
                "changes requested starts round 1",
                "pull-470",
                json!({"contexts": changes}),
                json!({"rounds": 0}),
                none.clone(),
                "fix",
                Some(1),
            ),
            (
                "failed required check starts round 2 on a new head",
                "pull-470",
                json!({"contexts": ci_failed}),
                json!({"rounds": 1, "sha": "older"}),
                none.clone(),
                "fix",
                Some(2),
            ),
            (
                "a failed check that is not required is ignored",
                "pull-470",
                json!({"contexts": {"rust": {"conclusion": "FAILURE"}}}),
                json!({"rounds": 0}),
                none.clone(),
                "merge",
                None,
            ),
            (
                "a fix already started for this head waits",
                "pull-470",
                json!({"contexts": changes}),
                json!({"rounds": 1, "sha": HEAD, "run": 7}),
                running.clone(),
                "wait",
                None,
            ),
            (
                "a fix that just succeeded gets time to show its push",
                "pull-470",
                json!({"contexts": changes}),
                json!({"rounds": 1, "sha": HEAD, "run": 7}),
                json!({"status": "completed", "conclusion": "success", "updated_at": "2026-10-12T01:58:00Z"}),
                "wait",
                None,
            ),
            (
                "a fix that succeeded without a push goes to a human",
                "pull-470",
                json!({"contexts": changes}),
                json!({"rounds": 1, "sha": HEAD, "run": 7}),
                json!({"status": "completed", "conclusion": "success", "updated_at": "2026-10-12T01:00:00Z"}),
                "needs-human",
                None,
            ),
            (
                "a fix run that just failed is blocked at once, with a round left",
                "pull-470",
                json!({"contexts": changes}),
                json!({"rounds": 1, "sha": HEAD, "run": 7}),
                failed_run.clone(),
                "needs-human",
                None,
            ),
            (
                "a cancelled fix run for a conflict is blocked at once",
                "pull-470",
                json!({"mergeable": CONFLICTING}),
                json!({"rounds": 1, "sha": HEAD, "run": 7}),
                json!({"status": "completed", "conclusion": "cancelled", "updated_at": "2026-10-12T01:59:30Z"}),
                "needs-human",
                None,
            ),
            (
                "a failed fix run whose push shows is the next head's business",
                "pull-470",
                json!({"contexts": changes}),
                json!({"rounds": 1, "sha": "older", "run": 7}),
                failed_run.clone(),
                "fix",
                Some(2),
            ),
            (
                "a fix with no run id times out to a human",
                "pull-470",
                json!({"contexts": changes}),
                json!({"rounds": 1, "sha": HEAD, "at": "2026-10-11T12:00:00Z"}),
                none.clone(),
                "needs-human",
                None,
            ),
            (
                "rounds exhausted goes to a human",
                "pull-470",
                json!({"contexts": changes}),
                json!({"rounds": 2, "sha": "older"}),
                none.clone(),
                "needs-human",
                None,
            ),
            (
                "needs-human is never touched again",
                "pull-470",
                json!({"labels": label, "contexts": changes}),
                json!({"rounds": 0}),
                none.clone(),
                "skip",
                None,
            ),
            // The markers needs-human leaves, once the label is gone again (tracker#511).
            (
                "removing needs-human starts again on the head the fix did not change",
                "pull-470",
                json!({"contexts": changes}),
                json!({"stopped": true, "rounds": 1, "sha": HEAD, "run": 7}),
                json!({"status": "completed", "updated_at": "2026-10-12T01:00:00Z"}),
                "fix",
                Some(1),
            ),
            (
                "removing needs-human gives used-up rounds back",
                "pull-470",
                json!({"contexts": changes}),
                json!({"stopped": true, "rounds": 2, "sha": "older"}),
                none.clone(),
                "fix",
                Some(1),
            ),
            (
                "removing needs-human retries a fix run that could not be started",
                "pull-470",
                json!({"contexts": changes}),
                json!({"stopped": true, "rounds": 1, "sha": HEAD, "at": "2026-10-12T01:59:00Z"}),
                none.clone(),
                "fix",
                Some(1),
            ),
            (
                "removing needs-human on a conflicting head rebases it",
                "pull-470",
                json!({"mergeable": CONFLICTING}),
                json!({"stopped": true, "rounds": 2, "sha": HEAD, "run": 7}),
                json!({"status": "completed", "updated_at": "2026-10-12T01:00:00Z"}),
                "fix",
                Some(1),
            ),
            (
                "a stopped pull request still labelled stays stopped",
                "pull-470",
                json!({"labels": label, "contexts": changes}),
                json!({"stopped": true, "rounds": 2, "sha": HEAD}),
                none.clone(),
                "skip",
                None,
            ),
            (
                "a stopped pull request that is approved and green merges",
                "pull-470",
                json!({}),
                json!({"stopped": true, "rounds": 2, "sha": HEAD}),
                none.clone(),
                "merge",
                None,
            ),
            (
                "a failing draft on our own branch is its author's",
                "pull-143",
                json!({}),
                json!({"rounds": 0}),
                none.clone(),
                "skip",
                None,
            ),
            (
                "a green draft on our own branch is its author's",
                "pull-143",
                json!({"contexts": {"ci": {"conclusion": "SUCCESS"}}}),
                json!({"rounds": 0}),
                none.clone(),
                "skip",
                None,
            ),
            (
                "another fork's dispatch/ branch gets no fix",
                "pull-470",
                json!({"headRepositoryOwner": someone, "contexts": changes}),
                json!({"rounds": 0}),
                none.clone(),
                "skip",
                None,
            ),
            (
                "an approved and green placeholder gets a fix for its messages, not a merge",
                "pull-511",
                json!({}),
                json!({"rounds": 0}),
                none.clone(),
                "fix",
                Some(1),
            ),
            (
                "a placeholder draft gets a fix, not ready",
                "pull-511",
                json!({"isDraft": true, "contexts": {"review": null}}),
                json!({"rounds": 0}),
                none.clone(),
                "fix",
                Some(1),
            ),
            (
                "a placeholder with a fix running waits",
                "pull-511",
                json!({}),
                json!({"rounds": 1, "sha": head_511, "run": 7}),
                running.clone(),
                "wait",
                None,
            ),
            (
                "a placeholder with the rounds used up goes to a human",
                "pull-511",
                json!({}),
                json!({"rounds": 2, "sha": "older"}),
                none.clone(),
                "needs-human",
                None,
            ),
            (
                "a placeholder title over reworded commits goes to a human",
                "pull-511",
                json!({"series": reworded}),
                json!({"rounds": 1, "sha": "older"}),
                none.clone(),
                "needs-human",
                None,
            ),
            (
                "a placeholder title on a draft is not marked ready",
                "pull-511",
                json!({"series": reworded, "isDraft": true}),
                json!({"rounds": 0}),
                none.clone(),
                "needs-human",
                None,
            ),
            (
                "a title the author wrote over reworded commits merges",
                "pull-511",
                json!({"series": reworded, "title": "check: Say why"}),
                json!({"rounds": 0}),
                none.clone(),
                "merge",
                None,
            ),
            (
                "a title that only mentions an agent run merges",
                "pull-470",
                json!({"title": "docs: Explain what Agent run 12 printed"}),
                json!({"rounds": 0}),
                none.clone(),
                "merge",
                None,
            ),
            (
                "a partial title is not merged",
                "pull-470",
                json!({"title": partial_title}),
                json!({"rounds": 0}),
                none.clone(),
                "needs-human",
                None,
            ),
            (
                "a partial description is not merged",
                "pull-470",
                json!({"body": partial_body}),
                json!({"rounds": 0}),
                none.clone(),
                "needs-human",
                None,
            ),
            (
                "a partial draft is not marked ready",
                "pull-470",
                json!({"title": partial_title, "body": partial_body, "isDraft": true, "contexts": {"review": null}}),
                json!({"rounds": 0}),
                none.clone(),
                "needs-human",
                None,
            ),
            (
                "a partial pull request with failed ci gets its fix first",
                "pull-470",
                json!({"title": partial_title, "contexts": ci_failed}),
                json!({"rounds": 0}),
                none.clone(),
                "fix",
                Some(1),
            ),
            (
                "another fork's placeholder is left alone",
                "pull-511",
                json!({"headRepositoryOwner": someone}),
                json!({"rounds": 0}),
                none.clone(),
                "skip",
                None,
            ),
            (
                "our own branch marked partial is its author's",
                "pull-143",
                json!({"title": partial_title, "isDraft": false, "contexts": {"ci": {"conclusion": "SUCCESS"}}}),
                json!({"rounds": 0}),
                none.clone(),
                "skip",
                None,
            ),
            // The verdict is a review of the head by the operator or the review job.
            (
                "an approval of an older head waits for one of this head",
                "pull-470",
                json!({"reviews": {"nodes": [review("APPROVED", job(), "older", "")]}}),
                json!({"rounds": 0}),
                none.clone(),
                "wait",
                None,
            ),
            (
                "a change request of an older head spends no round",
                "pull-470",
                json!({"reviews": {"nodes": [review("CHANGES_REQUESTED", job(), "older", "")]}}),
                json!({"rounds": 0}),
                none.clone(),
                "wait",
                None,
            ),
            (
                "a visitor's approval does not merge",
                "pull-470",
                json!({"reviews": {"nodes": [review("APPROVED", user("someone"), HEAD, "")]}}),
                json!({"rounds": 0}),
                none.clone(),
                "wait",
                None,
            ),
            (
                "a visitor's change request spends no round",
                "pull-470",
                json!({"reviews": {"nodes": [review("APPROVED", job(), HEAD, ""), review("CHANGES_REQUESTED", user("someone"), HEAD, "")]}}),
                json!({"rounds": 0}),
                none.clone(),
                "merge",
                None,
            ),
            (
                "the operator's approval after a change request merges",
                "pull-470",
                json!({"reviews": {"nodes": [review("CHANGES_REQUESTED", job(), HEAD, ""), review("APPROVED", user("cgwalters"), HEAD, "")]}}),
                json!({"rounds": 0}),
                none.clone(),
                "merge",
                None,
            ),
            (
                "the operator's change request after an approval gets a fix",
                "pull-470",
                json!({"reviews": {"nodes": [review("APPROVED", job(), HEAD, ""), review("CHANGES_REQUESTED", user("cgwalters"), HEAD, "")]}}),
                json!({"rounds": 0}),
                none.clone(),
                "fix",
                Some(1),
            ),
            (
                "a withheld approval waits for a human",
                "pull-470",
                json!({"reviews": {"nodes": [review("COMMENTED", job(), HEAD, &format!("{WITHHELD}\n\nVERDICT: APPROVE"))]}}),
                json!({"rounds": 0}),
                none.clone(),
                "wait",
                None,
            ),
            (
                "the operator's approval over a withheld one merges",
                "pull-470",
                json!({"reviews": {"nodes": [review("COMMENTED", job(), HEAD, WITHHELD), review("APPROVED", user("cgwalters"), HEAD, "")]}}),
                json!({"rounds": 0}),
                none.clone(),
                "merge",
                None,
            ),
            (
                "a dismissed approval counts for nothing",
                "pull-470",
                json!({"reviews": {"nodes": [review("DISMISSED", job(), HEAD, "")]}}),
                json!({"rounds": 0}),
                none.clone(),
                "wait",
                None,
            ),
            // Shared bot identity alone cannot identify the review workflow.
            (
                "an unauthenticated Actions workflow approval waits",
                "pull-470",
                json!({"reviews": {"nodes": [review("APPROVED", json!({"__typename": "Bot", "login": "github-actions[bot]"}), HEAD, "")]}}),
                json!({"rounds": 0}),
                none.clone(),
                "wait",
                None,
            ),
            (
                "a user named like the review job does not merge",
                "pull-470",
                json!({"reviews": {"nodes": [review("APPROVED", user("github-actions"), HEAD, "")]}}),
                json!({"rounds": 0}),
                none.clone(),
                "wait",
                None,
            ),
            (
                "an app named like the operator does not merge",
                "pull-470",
                json!({"reviews": {"nodes": [review("APPROVED", json!({"__typename": "Bot", "login": "cgwalters"}), HEAD, "")]}}),
                json!({"rounds": 0}),
                none.clone(),
                "wait",
                None,
            ),
            (
                "another bot's change request spends no round",
                "pull-470",
                json!({"reviews": {"nodes": [review("APPROVED", job(), HEAD, ""), review("CHANGES_REQUESTED", json!({"__typename": "Bot", "login": "dependabot"}), HEAD, "")]}}),
                json!({"rounds": 0}),
                none.clone(),
                "merge",
                None,
            ),
            // What a query did not read whole is not decided on (approved and green otherwise).
            (
                "a needs-human label past the labels read waits",
                "pull-470",
                json!({"truncated": {"labels": true}}),
                json!({"rounds": 0}),
                none.clone(),
                "wait",
                None,
            ),
            (
                "a change request before the reviews read waits",
                "pull-470",
                json!({"truncated": {"reviews": true}}),
                json!({"rounds": 0}),
                none.clone(),
                "wait",
                None,
            ),
            (
                "a placeholder subject past the commits read waits",
                "pull-470",
                json!({"truncated": {"commits": true}}),
                json!({"rounds": 0}),
                none.clone(),
                "wait",
                None,
            ),
            (
                "a failed check past the checks read waits",
                "pull-470",
                json!({"truncated": {"checks": true}}),
                json!({"rounds": 0}),
                none.clone(),
                "wait",
                None,
            ),
            (
                "a connection without pageInfo is not known whole",
                "pull-470",
                json!({"truncated": {"checks": null}}),
                json!({"rounds": 0}),
                none.clone(),
                "wait",
                None,
            ),
            (
                "a truncated draft is not marked ready",
                "pull-470",
                json!({"isDraft": true, "contexts": {"review": null}, "truncated": {"checks": true}}),
                json!({"rounds": 0}),
                none.clone(),
                "wait",
                None,
            ),
            (
                "a truncated pull request labelled needs-human is still left alone",
                "pull-470",
                json!({"labels": label, "truncated": {"labels": true}}),
                json!({"rounds": 0}),
                none.clone(),
                "skip",
                None,
            ),
        ];
        for (name, fixture, change, state, fix_run, action, round) in cases {
            let p = pull(fixture, change);
            let got = decide(
                &config(),
                &p,
                &classify_checks(&p, &required()),
                &marker(state),
                Some(&fix_run).filter(|r| !r.is_null()),
                now(),
            );
            assert_eq!(
                (got.action, got.round),
                (action, round),
                "{name}: {}",
                got.reason
            );
        }
    }

    #[test]
    fn in_flight_table() {
        let changes = json!({"contexts": {"review": {"state": "FAILURE"}}});
        for (name, fixture, change, action, expected) in [
            (
                "a dispatch pull request that waits",
                "pull-470",
                json!({}),
                "wait",
                true,
            ),
            (
                "one a fix was just started for",
                "pull-470",
                changes.clone(),
                "fix",
                true,
            ),
            (
                "one with auto-merge on, until it has merged",
                "pull-470",
                json!({"autoMergeRequest": {"enabledAt": "x"}}),
                "skip",
                true,
            ),
            (
                "one this pass handed to a human",
                "pull-470",
                changes,
                "needs-human",
                false,
            ),
            (
                "one labelled needs-human",
                "pull-470",
                json!({"labels": {"nodes": [{"name": LABEL}]}}),
                "skip",
                false,
            ),
            (
                "another fork's dispatch/ branch",
                "pull-470",
                json!({"headRepositoryOwner": {"login": "someone"}}),
                "skip",
                false,
            ),
            (
                "a branch of the repository itself",
                "pull-143",
                json!({}),
                "skip",
                false,
            ),
            (
                "the bot's own pull request being merged",
                "pull-143",
                json!({"isDraft": false}),
                "merge",
                false,
            ),
        ] {
            assert_eq!(
                in_flight(&pull(fixture, change), BOT, action),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn required_checks_come_from_the_rules_as_well_as_is_required() {
        // Recorded: GitHub reports isRequired false for `ci` on this draft.
        let p = pull("pull-143", json!({}));
        let names = |checks: Checks| {
            checks
                .failed
                .into_iter()
                .map(|(n, _)| n)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(classify_checks(&p, &required())), ["ci"]);
        // With no rule and no flag, every check counts.
        assert!(classify_checks(&p, &[]).failed.len() > 1);
        // The review status is not a check: the review of the head is read instead.
        let p = pull(
            "pull-470",
            json!({"contexts": {"review": {"state": "FAILURE"}}}),
        );
        assert_eq!(classify_checks(&p, &[]), Checks::default());
        // A ruleset that requires it neither waits on it nor counts every check.
        let p = pull(
            "pull-143",
            json!({"contexts": {"ci": {"conclusion": "SUCCESS"}}}),
        );
        for rules in [vec![json!("review")], vec!["ci".into(), "review".into()]] {
            assert_eq!(classify_checks(&p, &rules), Checks::default(), "{rules:?}");
        }
    }

    #[test]
    fn required_checks_table() {
        let ruleset = json!({"context": "ci", "integration_id": 15368});
        let classic = |rest: Value| {
            let mut b = protected(json!(null));
            b["protection"]["required_status_checks"] = rest;
            b
        };
        for (name, branch, expected) in [
            (
                "unprotected",
                json!({"protected": false}),
                Some(vec![ruleset.clone()]),
            ),
            (
                "classic checks are added to the rulesets'",
                protected(
                    json!([{"context": "build", "app_id": 15368}, {"context": "lint", "app_id": null}]),
                ),
                Some(vec![
                    ruleset.clone(),
                    json!({"context": "build", "integration_id": 15368}),
                    json!({"context": "lint", "integration_id": null}),
                ]),
            ),
            (
                "contexts without checks bind no app",
                classic(json!({"enforcement_level": "everyone", "contexts": ["build"]})),
                Some(vec![ruleset.clone(), json!({"context": "build"})]),
            ),
            (
                "enforcement off",
                classic(json!({"enforcement_level": "off", "contexts": ["build"]})),
                Some(vec![ruleset.clone()]),
            ),
            (
                "protection disabled",
                json!({"protected": true, "protection": {"enabled": false}}),
                Some(vec![ruleset.clone()]),
            ),
            ("protection not reported", json!({"protected": true}), None),
            ("requirements not reported", classic(json!(null)), None),
            (
                "required list not reported",
                classic(json!({"enforcement_level": "everyone"})),
                None,
            ),
            ("protected unknown", json!({"name": "main"}), None),
        ] {
            assert_eq!(
                required_checks(&fixture("rules"), &branch).ok(),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn an_absent_classic_required_check_holds_an_approved_head() {
        // Every check that reported passed, `ci` included; `build` never reported.
        let p = pull("pull-470", json!({}));
        for (name, rules, branch) in [
            (
                "no ruleset",
                json!([]),
                protected(json!([{"context": "build", "app_id": null}])),
            ),
            (
                "beside a ruleset's passed check",
                fixture("rules"),
                protected(json!([{"context": "build", "app_id": 15368}])),
            ),
        ] {
            let checks = classify_checks(&p, &required_checks(&rules, &branch).unwrap());
            assert_eq!(checks.pending, ["build"], "{name}");
            let got = decide(&config(), &p, &checks, &Marker::default(), None, now());
            assert_eq!(got.action, "wait", "{name}: {}", got.reason);
        }
        // Even with a null rollup.
        let mut p = p;
        p["commits"]["nodes"][0]["commit"]["statusCheckRollup"] = Value::Null;
        let checks = classify_checks(
            &p,
            &required_checks(
                &json!([]),
                &protected(json!([{"context": "build", "app_id": null}])),
            )
            .unwrap(),
        );
        assert_eq!(checks.pending, ["build"]);
    }

    #[test]
    fn wrong_integration_cannot_supply_required_check() {
        let p = pull(
            "pull-143",
            json!({"contexts": {"ci": {
                "conclusion": "SUCCESS", "isRequired": false,
                "checkSuite": {"app": {"databaseId": 999}}
            }}}),
        );
        let checks = classify_checks(&p, &required());
        assert_eq!(checks.pending, ["ci"]);
        assert!(checks.failed.is_empty());
    }

    #[test]
    fn verdicts_follow_submission_not_creation_order() {
        let mut later = review("CHANGES_REQUESTED", job(), HEAD, "");
        later["submittedAt"] = json!("2026-10-12T01:30:00Z");
        let earlier = review("APPROVED", job(), HEAD, "");
        for reviews in [json!([later, earlier]), json!([earlier, later])] {
            let p = pull("pull-470", json!({"reviews": {"nodes": reviews}}));
            assert_eq!(
                review_at_head(&p, "cgwalters"),
                Some(Review::ChangesRequested)
            );
            let checks = classify_checks(&p, &required());
            assert_eq!(
                decide(&config(), &p, &checks, &Marker::default(), None, now()).action,
                "fix"
            );
        }
    }

    #[test]
    fn recorded_job_only_verdicts_wait() {
        for status in ["SUCCESS", "FAILURE"] {
            let p = pull_with_author(
                "pull-470",
                json!({"contexts": {"review": {"state": status}}}),
                json!({"__typename": "Bot", "login": "github-actions[bot]"}),
            );
            assert_eq!(review_at_head(&p, "cgwalters"), None);
            let checks = classify_checks(&p, &required());
            assert_eq!(
                decide(&config(), &p, &checks, &Marker::default(), None, now()).action,
                "wait"
            );
        }
    }

    #[test]
    fn another_workflows_review_is_not_a_job_verdict() {
        for login in ["github-actions", "github-actions[bot]"] {
            for (state, body) in [
                ("APPROVED", "VERDICT: APPROVE"),
                ("CHANGES_REQUESTED", "VERDICT: CHANGES"),
                ("COMMENTED", WITHHELD),
            ] {
                let p = pull(
                    "pull-470",
                    json!({"reviews": {"nodes": [review(
                        state, json!({"__typename": "Bot", "login": login}), HEAD, body
                    )]}}),
                );
                assert_eq!(review_at_head(&p, "cgwalters"), None);
            }
        }
    }

    #[test]
    fn a_pull_request_not_read_whole_waits() {
        let mut p = pull("pull-470", json!({}));
        assert!(truncated(&p).is_empty());
        p["commits"]["nodes"][0]["commit"]["statusCheckRollup"] = Value::Null;
        assert!(truncated(&p).is_empty());
        // A missing rollup is not a null one.
        p["commits"]["nodes"][0]["commit"]
            .as_object_mut()
            .unwrap()
            .remove("statusCheckRollup");
        assert_eq!(truncated(&p), ["checks"]);
        // With nothing required, 100 green checks over a 101st that failed
        // do not merge.
        let mut p = pull("pull-470", json!({"truncated": {"checks": true}}));
        let contexts = p
            .pointer_mut("/commits/nodes/0/commit/statusCheckRollup/contexts/nodes")
            .unwrap();
        *contexts = (0..100)
            .map(|i| json!({"__typename": "CheckRun", "name": format!("c{i}"), "status": "COMPLETED", "conclusion": "SUCCESS", "isRequired": false}))
            .collect();
        let got = decide(
            &config(),
            &p,
            &classify_checks(&p, &[]),
            &Marker::default(),
            None,
            now(),
        );
        assert_eq!(got.action, "wait", "{}", got.reason);
    }

    #[test]
    fn only_the_bots_marker_counts() {
        let state = json!({"rounds": 1, "sha": HEAD, "run": 42});
        let mine = json!({"id": 2, "user": {"login": BOT}, "body": marker_body(&state, "Fix round 1 of 2.")});
        let forged = json!({"id": 1, "user": {"login": "visitor"}, "body": marker_body(&json!({"rounds": 0}), "reset")});
        let expected = Marker {
            id: Some(2),
            rounds: 1,
            sha: Some(HEAD.into()),
            run: Some(42),
            ..Marker::default()
        };
        assert_eq!(find_marker(&[forged.clone(), mine.clone()], BOT), expected);
        assert_eq!(find_marker(&[forged], BOT), Marker::default());
        let recorded = fixture("comments-470");
        assert_eq!(
            find_marker(recorded.as_array().unwrap(), BOT),
            Marker::default()
        );
        // Malformed markers are not ours after all; a later valid one is.
        for body in [
            marker_body(&json!({"rounds": -1}), ""),
            marker_body(&json!({"rounds": "1"}), ""),
            marker_body(&json!({"rounds": 1u64 << 53}), ""),
            format!("<!-- {MARKER}: {{\"rounds\": 1 -->"),
            format!("<!-- {MARKER}: {{\"rounds\":\n1}} -->"),
            // poll.cjs reads the first marker of a comment only.
            format!(
                "<!-- {MARKER}: {{rounds: 1}} -->\n{}",
                marker_body(&json!({"rounds": 0}), "")
            ),
        ] {
            let bad = json!({"id": 3, "user": {"login": BOT}, "body": body});
            assert_eq!(
                find_marker(std::slice::from_ref(&bad), BOT),
                Marker::default(),
                "{body}"
            );
            assert_eq!(find_marker(&[bad, mine.clone()], BOT), expected, "{body}");
        }
    }

    #[test]
    fn a_blocked_fix_run_is_named_in_the_reason() {
        let p = pull(
            "pull-470",
            json!({"contexts": {"review": {"state": "FAILURE"}}}),
        );
        let run = json!({"status": "completed", "conclusion": "failure", "updated_at": "2026-10-12T01:59:30Z"});
        let got = decide(
            &config(),
            &p,
            &classify_checks(&p, &required()),
            &marker(json!({"rounds": 1, "sha": HEAD, "run": 7})),
            Some(&run),
            now(),
        );
        assert_eq!(got.action, "needs-human");
        assert!(
            got.reason.starts_with("fix round 1 is blocked: "),
            "{}",
            got.reason
        );
        assert!(
            got.reason.contains(
                "https://github.com/cgwalters-forge/tracker/actions/runs/7 ended (failure)"
            ),
            "{}",
            got.reason
        );
    }

    #[test]
    fn idle_backoff_doubles_to_four_passes_a_day() {
        let mut interval = MIN_INTERVAL_SECONDS;
        let mut seen = vec![];
        for _ in 0..9 {
            interval = next_interval(interval, false);
            seen.push(interval);
        }
        assert_eq!(
            seen,
            [600, 1200, 2400, 4800, 9600, 19200, 21600, 21600, 21600]
        );
        assert_eq!(
            next_interval(MAX_INTERVAL_SECONDS, true),
            MIN_INTERVAL_SECONDS
        );
        assert_eq!(MAX_INTERVAL_SECONDS * 4, 24 * 60 * 60);
    }

    #[test]
    fn repository_and_link_table() {
        for (repo, valid) in [
            ("cgwalters-forge/agent-board", true),
            ("containers/bootc", true),
            ("o/.github", true),
            ("o/..", false),
            ("o/r/issues", false),
            ("o/r?x=1", false),
            ("-o/r", false),
            ("o", false),
            ("o/", false),
        ] {
            assert_eq!(validate_repository(repo).is_ok(), valid, "{repo}");
        }
        let link = r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=5>; rel="last""#;
        assert_eq!(
            next_link(link).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
        assert_eq!(
            next_link(r#"<https://api.github.com/x?page=1>; rel="prev""#),
            None
        );
    }

    #[test]
    fn state_table() {
        let good = json!({"schema": POLL_STATE_SCHEMA, "interval_seconds": 600,
            "responses": {"https://api.github.com/repos/o/r/events?per_page=100": {"etag": "\"a\"", "body": []}}});
        let state = State::from_json(&good).unwrap();
        assert_eq!(state.to_json(), good);
        for (field, value) in [
            ("schema", json!("agent-board/poll-state/v0")),
            ("interval_seconds", json!(60)),
            ("interval_seconds", json!(MAX_INTERVAL_SECONDS + 1)),
            ("responses", json!({"https://evil.example/": {"body": []}})),
            (
                "responses",
                json!({"https://api.github.com/x": {"etag": "\"a\""}}),
            ),
            ("unknown", json!(1)),
        ] {
            let mut bad = good.clone();
            bad[field] = value;
            assert!(State::from_json(&bad).is_err(), "{field}");
        }
    }

    type Page = (
        Option<&'static str>,
        Option<&'static str>,
        Option<String>,
        Value,
    );

    /// GitHub as far as a pass reads it: conditional GETs by URL, and the
    /// pull request query. It records each URL and the validator sent.
    #[derive(Default)]
    struct Fake {
        /// ETag, Last-Modified, next page and body, by URL.
        rest: BTreeMap<String, Page>,
        pulls: BTreeMap<u64, Value>,
        /// Leave `Link` out of 304s, as a response without metadata may.
        bare_304: bool,
        sent: RefCell<Vec<(String, Option<String>)>>,
    }

    impl Fake {
        fn page(
            &mut self,
            path: &str,
            etag: Option<&'static str>,
            modified: Option<&'static str>,
            body: Value,
        ) {
            self.rest
                .insert(format!("{API_ROOT}{path}"), (etag, modified, None, body));
        }
    }

    impl Rest for Fake {
        fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
            self.sent
                .borrow_mut()
                .push((url.into(), validator.map(|(n, v)| format!("{n}: {v}"))));
            let (etag, modified, next, body) = self
                .rest
                .get(url)
                .with_context(|| format!("unexpected GET {url}"))?;
            let fresh = match validator {
                Some(("If-None-Match", v)) => Some(v) != *etag,
                Some(("If-Modified-Since", v)) => Some(v) != *modified,
                _ => true,
            };
            Ok(RestResponse {
                status: if fresh { 200 } else { 304 },
                etag: etag.map(str::to_owned),
                last_modified: modified.map(str::to_owned),
                link: next
                    .as_ref()
                    .filter(|_| fresh || !self.bare_304)
                    .map(|n| format!("<{n}>; rel=\"next\"")),
                body: if fresh { body.clone() } else { Value::Null },
            })
        }
    }

    impl Transport for Fake {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            assert_eq!(query, PULL_QUERY);
            let number = variables["number"].as_u64().unwrap();
            Ok(json!({"repository": {"pullRequest": self.pulls[&number]}}))
        }
    }

    fn event(id: u64, kind: &str, login: &str, payload: Value) -> Value {
        json!({"id": id.to_string(), "type": kind, "actor": {"login": login, "avatar_url": "x"}, "payload": payload, "created_at": "2026-10-12T01:00:00Z"})
    }

    const REPO: &str = "cgwalters-forge/agentic-job";

    fn github() -> Fake {
        let mut fake = Fake::default();
        fake.page(
            &format!("repos/{REPO}/events?per_page=100"),
            Some("\"e1\""),
            None,
            json!([
                event(12, "PushEvent", BOT, json!({"ref": "refs/heads/dispatch/x", "commits": [{"sha": "a"}]})),
                event(11, "PullRequestReviewEvent", "cgwalters", json!({"action": "created", "review": {"state": "approved"}, "pull_request": {"number": 537}})),
                event(10, "IssueCommentEvent", "someone", json!({"action": "created", "issue": {"number": 40}})),
                event(9, "WatchEvent", "cgwalters", json!({"action": "started"})),
                event(8, "IssuesEvent", BOT, json!({"action": "labeled", "label": {"name": LABEL}, "issue": {"number": 40}})),
            ]),
        );
        fake.page(
            &format!("repos/{REPO}/pulls?state=open&per_page=100"),
            Some("\"p1\""),
            None,
            json!([{"number": 470, "user": {"login": BOT}}, {"number": 5, "user": {"login": "someone"}}]),
        );
        // Without an ETag, the pass falls back to Last-Modified.
        fake.page(
            &format!("repos/{REPO}/rules/branches/main"),
            None,
            Some("Mon, 12 Oct 2026 00:00:00 GMT"),
            fixture("rules"),
        );
        fake.page(
            &format!("repos/{REPO}/branches/main"),
            Some("\"b1\""),
            None,
            json!({"name": "main", "protected": false}),
        );
        let comments = format!("{API_ROOT}repos/{REPO}/issues/470/comments?per_page=100");
        fake.rest.insert(
            comments.clone(),
            (
                Some("\"c1\""),
                None,
                Some(format!("{comments}&page=2")),
                fixture("comments-470"),
            ),
        );
        fake.page(
            &format!("repos/{REPO}/issues/470/comments?per_page=100&page=2"),
            Some("\"c2\""),
            None,
            json!([]),
        );
        fake.pulls.insert(470, pull("pull-470", json!({})));
        fake
    }

    fn pass(fake: &Fake, state: State, watch: &[&str], follow: &[&str]) -> Outcome {
        let owned = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        run(
            fake,
            fake,
            &config(),
            state,
            &owned(watch),
            &owned(follow),
            now(),
        )
        .unwrap()
    }

    #[test]
    fn a_pass_reads_activity_and_decides_with_cached_requests() {
        let mut fake = github();
        let first = pass(&fake, State::default(), &[], &[REPO]);
        assert_eq!(
            first.lines,
            [
                "cgwalters-forge/agentic-job#40 activity: cgwalters-bot labeled needs-human (state)",
                "cgwalters-forge/agentic-job#537 activity: cgwalters reviewed (approved) (direction)",
                "cgwalters-forge/agentic-job activity: cgwalters-bot pushed refs/heads/dispatch/x (state)",
                "cgwalters-forge/agentic-job#470 488f45ebacd0 merge: review approved and checks green",
            ]
        );
        assert_eq!((first.errors, first.activity, first.in_flight), (0, 3, 1));
        assert_eq!(first.state.interval_seconds, MIN_INTERVAL_SECONDS);
        assert!(
            fake.sent
                .borrow()
                .iter()
                .all(|(_, validator)| validator.is_none())
        );
        // Events keep only what the pass reads.
        let events = &first.state.responses[&format!("{API_ROOT}repos/{REPO}/events?per_page=100")];
        assert_eq!(
            events["body"][0]["payload"],
            json!({"action": null, "ref": "refs/heads/dispatch/x", "number": null, "review": null, "label": null})
        );

        // Nothing changed: every GET is revalidated and answered 304 from the
        // saved state, and the decision is the same.
        fake.sent.borrow_mut().clear();
        let saved = State::from_json(&first.state.to_json()).unwrap();
        let second = pass(&fake, saved, &[], &[REPO]);
        assert_eq!(second.lines, first.lines[3..]);
        let sent = fake.sent.borrow().clone();
        assert_eq!(sent.len(), 6);
        assert!(
            sent.iter().all(|(_, validator)| validator.is_some()),
            "{sent:?}"
        );
        assert!(
            sent.iter()
                .any(|(_, v)| v.as_deref()
                    == Some("If-Modified-Since: Mon, 12 Oct 2026 00:00:00 GMT"))
        );
        assert_eq!(second.state.responses, first.state.responses);

        // New activity is read, an event GitHub published late under an older
        // id included; events already read are not repeated.
        let events = fake
            .rest
            .get_mut(&format!("{API_ROOT}repos/{REPO}/events?per_page=100"))
            .unwrap();
        events.0 = Some("\"e2\"");
        let mut list = events.3.as_array().unwrap().clone();
        list.push(event(
            7,
            "IssueCommentEvent",
            "cgwalters",
            json!({"issue": {"number": 470}}),
        ));
        events.3 = Value::Array(list);
        let third = pass(&fake, second.state, &[REPO], &[]);
        assert_eq!(
            third.lines,
            ["cgwalters-forge/agentic-job#470 activity: cgwalters commented (direction)"]
        );
        // Only what this pass used is kept.
        assert_eq!(third.state.responses.len(), 1);
    }

    #[test]
    fn classic_protection_is_read_and_unreadable_protection_is_an_error() {
        let mut fake = github();
        fake.page(
            &format!("repos/{REPO}/branches/main"),
            None,
            None,
            protected(json!([{"context": "build", "app_id": null}])),
        );
        let outcome = pass(&fake, State::default(), &[], &[REPO]);
        assert_eq!(
            outcome.lines.last().unwrap(),
            "cgwalters-forge/agentic-job#470 488f45ebacd0 wait: checks pending: build"
        );
        fake.page(
            &format!("repos/{REPO}/branches/main"),
            None,
            None,
            json!({"protected": true}),
        );
        let outcome = pass(&fake, State::default(), &[], &[REPO]);
        assert_eq!(outcome.errors, 1);
        assert_eq!(
            outcome.lines.last().unwrap(),
            "cgwalters-forge/agentic-job#470 error: branch protection's required checks are unreadable"
        );
    }

    #[test]
    fn a_list_that_grows_past_its_page_is_followed_on_a_304() {
        let mut fake = github();
        let comments = format!("{API_ROOT}repos/{REPO}/issues/470/comments?per_page=100");
        fake.rest.get_mut(&comments).unwrap().2 = None;
        let first = pass(&fake, State::default(), &[], &[REPO]);
        // The first page is unchanged, but its Link names a second one now.
        fake.rest.get_mut(&comments).unwrap().2 = Some(format!("{comments}&page=2"));
        fake.sent.borrow_mut().clear();
        pass(&fake, first.state, &[], &[REPO]);
        assert!(
            fake.sent
                .borrow()
                .iter()
                .any(|(url, _)| *url == format!("{comments}&page=2"))
        );
    }

    #[test]
    fn a_304_without_link_keeps_the_cached_pages() {
        let mut fake = github();
        let comments = format!("{API_ROOT}repos/{REPO}/issues/470/comments?per_page=100");
        let second = format!("{comments}&page=2");
        fake.rest.get_mut(&comments).unwrap().2 = Some(second.clone());
        fake.rest
            .insert(second.clone(), (Some("\"c2\""), None, None, json!([])));
        let first = pass(&fake, State::default(), &[], &[REPO]);
        fake.bare_304 = true;
        fake.sent.borrow_mut().clear();
        let outcome = pass(&fake, first.state, &[], &[REPO]);
        assert!(fake.sent.borrow().iter().any(|(url, _)| *url == second));
        assert!(outcome.state.responses.contains_key(&second));
    }

    #[test]
    fn idle_passes_back_off_and_a_change_resets_the_interval() {
        let fake = github();
        let mut state = pass(&fake, State::default(), &[REPO], &[]).state;
        assert_eq!(state.interval_seconds, MIN_INTERVAL_SECONDS);
        let mut seen = vec![];
        for _ in 0..3 {
            let idle = pass(&fake, state, &[REPO], &[]);
            assert!(idle.lines.is_empty(), "{:?}", idle.lines);
            seen.push(idle.state.interval_seconds);
            state = idle.state;
        }
        assert_eq!(seen, [600, 1200, 2400]);
        // A pull request in flight keeps it short, as poll.yml's chain does.
        assert_eq!(
            pass(&fake, state, &[REPO], &[REPO]).state.interval_seconds,
            MIN_INTERVAL_SECONDS
        );
    }

    #[test]
    fn unreadable_activity_and_pages_count_as_errors() {
        let mut fake = github();
        // A full page of events none of which the last pass had: the gap is reported.
        let events: Vec<Value> = (200..300)
            .rev()
            .map(|id| {
                event(
                    id,
                    "IssueCommentEvent",
                    "someone",
                    json!({"issue": {"number": 1}}),
                )
            })
            .collect();
        fake.page(
            &format!("repos/{REPO}/events?per_page=100"),
            Some("\"e3\""),
            None,
            Value::Array(events),
        );
        // A Link header must not lead the token out of the API.
        let comments = format!("{API_ROOT}repos/{REPO}/issues/470/comments?per_page=100");
        fake.rest.get_mut(&comments).unwrap().2 = Some("https://evil.example/comments".into());
        let state = State {
            interval_seconds: MAX_INTERVAL_SECONDS,
            responses: [(
                format!("{API_ROOT}repos/{REPO}/events?per_page=100"),
                json!({"etag": "\"e0\"", "body": [{"id": "7"}]}),
            )]
            .into(),
        };
        let outcome = pass(&fake, state, &[], &[REPO]);
        assert_eq!(
            outcome.lines,
            [
                "cgwalters-forge/agentic-job activity: more than 100 events since the last pass; older ones were not read",
                "cgwalters-forge/agentic-job#470 error: pagination left the API: https://evil.example/comments",
            ]
        );
        assert_eq!(outcome.errors, 1);
        assert_eq!(outcome.state.interval_seconds, MIN_INTERVAL_SECONDS);
        assert!(
            !fake
                .sent
                .borrow()
                .iter()
                .any(|(url, _)| !url.starts_with(API_ROOT))
        );
        // An unknown repository fails that repository only.
        let outcome = pass(&fake, State::default(), &["o/missing"], &[]);
        assert_eq!(outcome.errors, 1);
        assert!(
            outcome.lines[0].starts_with("o/missing error: unexpected GET"),
            "{:?}",
            outcome.lines
        );
    }

    #[test]
    fn invalid_inputs_are_refused_before_any_request() {
        let fake = github();
        let owned = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let many: Vec<String> = (0..=POLL_REPOSITORY_LIMIT)
            .map(|i| format!("o/r{i}"))
            .collect();
        for (watch, operator, dispatch) in [
            (owned(&[]), "cgwalters", "cgwalters-forge/tracker"),
            (owned(&["o/r/../x"]), "cgwalters", "cgwalters-forge/tracker"),
            (owned(&[REPO]), "cg walters", "cgwalters-forge/tracker"),
            (owned(&[REPO]), "cgwalters", "tracker"),
            (many, "cgwalters", "cgwalters-forge/tracker"),
        ] {
            let config = Config {
                operator: operator.into(),
                bot: BOT.into(),
                dispatch_repo: dispatch.into(),
            };
            assert!(run(&fake, &fake, &config, State::default(), &watch, &[], now()).is_err());
        }
        assert!(fake.sent.borrow().is_empty());
    }
}
