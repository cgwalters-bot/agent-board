//! `ask create`, `ask list` and `ask resolve`: a question for the operator is an issue
//! labelled `question` and `needs-human`, assigned to them and added to the board; the
//! item it blocks is labelled `needs-human` too. That label is the one marker that the
//! operator must act: the pass's stage 5 derives Needs human from it, and Done from the
//! closed ask, so neither command writes Status. Resolving comments the answer, closes
//! the ask and takes `needs-human` off the blocked item, unless another open ask on the
//! board names that item on its `Blocks:` line. The asks are read from the board, not
//! from search, which may lag a just-opened one. Since anyone who can edit an ask can
//! change that line, the label only comes off an item on the board, and stays where
//! the board cannot be read whole, said on stderr. Resolving an ask that is already
//! closed only finishes taking the label off, so a failed unlabel can be retried
//! without posting the answer twice. This follows homegit's `bot-board question` and
//! `resolve`. Like `item
//! add`/`item edit`, both print proposals unless `--apply` is given, and only then write
//! with the operator's own token. No workflow here passes `--apply`, and an agent holds
//! no token.
use anyhow::{Context, Result, ensure};
use board_core::{ItemRef, NEEDS_HUMAN_LABEL, Snapshot};
use board_forge::github::{GitHub, Transport};
use clap::Args;
use serde_json::{Value, json};
use std::io::Write;

/// The label that marks an issue as an ask; it must already exist in the repository,
/// as must `needs-human` there and beside the blocked item.
pub const ASK_LABEL: &str = "question";
/// The first line of an ask names the item it blocks.
const BLOCKS: &str = "Blocks: ";
/// Options are lettered A to J, as in homegit.
const OPTION_LETTERS: &str = "ABCDEFGHIJ";
/// homegit's default title length: the question flattened to one line.
const TITLE_CHARS: usize = 100;

#[derive(Args)]
pub struct Create {
    /// The question to ask.
    pub question: String,
    /// The board issue the question blocks: URL, OWNER/REPO#N, or N with --repo.
    #[arg(long)]
    pub item: String,
    /// The operator's GitHub login; the ask is assigned to them.
    #[arg(long)]
    pub assignee: String,
    /// An alternative to choose from, lettered A, B, ... in order (repeat 2 to 10 times).
    #[arg(long = "option", value_name = "TEXT")]
    pub options: Vec<String>,
    /// Recommend the first option (A), for this reason.
    #[arg(long, value_name = "WHY")]
    pub recommend: Option<String>,
    /// Background printed before the question.
    #[arg(long)]
    pub context: Option<String>,
    /// Issue title; defaults to the question on one line, cut to 100 characters.
    #[arg(long)]
    pub title: Option<String>,
    /// Open the issue, add it to the board and label the item with your token; without it, only print proposals.
    #[arg(long)]
    pub apply: bool,
}

#[derive(Args)]
pub struct Resolve {
    #[command(flatten)]
    pub settings: crate::decisions::Settings,
    /// The ask: URL, OWNER/REPO#N, or N with --repo.
    pub item: String,
    /// The answer, posted as a comment before the ask is closed.
    pub answer: String,
    /// Comment, close and unlabel with your token; without it, only print proposals.
    #[arg(long)]
    pub apply: bool,
}

/// The ask's title and body. Validation here precedes every read and write, and the
/// token.
pub fn compose(args: &Create, blocked: &ItemRef, repository: &str) -> Result<(String, String)> {
    let question = args.question.trim();
    ensure!(!question.is_empty(), "the question is empty");
    board_forge::github_item_url(&ItemRef {
        repository: repository.into(),
        ..blocked.clone()
    })
    .context("invalid ask repository")?;
    ensure!(
        board_forge::is_github_login(&args.assignee),
        "invalid GitHub login {:?}",
        args.assignee
    );
    ensure!(
        args.options.is_empty() || (2..=OPTION_LETTERS.len()).contains(&args.options.len()),
        "give no options or 2 to {} --option values",
        OPTION_LETTERS.len()
    );
    ensure!(
        args.recommend.is_none() || !args.options.is_empty(),
        "--recommend needs options; the recommended one goes first as A"
    );
    let title = match &args.title {
        Some(title) => title.trim().to_owned(),
        None => question
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(TITLE_CHARS)
            .collect(),
    };
    ensure!(!title.is_empty(), "the title is empty");
    ensure!(!title.contains('\n'), "the title must be one line");
    let url = board_forge::github_item_url(blocked)?;
    // A bare URL to another repository puts a "mentioned this" event on that timeline,
    // which may be an upstream project's; a code span links nothing.
    let mut body = if blocked.repository.eq_ignore_ascii_case(repository) {
        format!("{BLOCKS}{url}\n\n")
    } else {
        format!("{BLOCKS}`{url}`\n\n")
    };
    if let Some(context) = args.context.as_deref().map(str::trim)
        && !context.is_empty()
    {
        body.push_str(&format!("{context}\n\n"));
    }
    body.push_str(&format!("Q: {question}\n\n"));
    if !args.options.is_empty() {
        body.push_str("Options:\n");
        for (letter, option) in OPTION_LETTERS.chars().zip(&args.options) {
            let option = option.trim();
            // One line each, so the lettered list reads as the operator's choices.
            ensure!(
                !option.is_empty() && !option.contains('\n'),
                "option {letter} must be one non-empty line"
            );
            body.push_str(&format!("{letter}) {option}\n"));
        }
        body.push('\n');
    }
    if let Some(why) = &args.recommend {
        let why = why.trim();
        ensure!(!why.is_empty(), "--recommend needs a reason");
        body.push_str(&format!("Recommended: A, because {why}\n\n"));
    }
    body.push_str(&format!(
        "Reply with {} free text. Only comments by @{} count.\n",
        if args.options.is_empty() {
            "your answer in"
        } else {
            "a letter or"
        },
        args.assignee
    ));
    Ok((title, body))
}

/// A gh-aw `add_labels` or `remove_labels` proposal of `needs-human`.
fn needs_human(kind: &str, item: &ItemRef) -> Value {
    json!({"type": kind, "target_repo": item.repository, "item_number": item.number, "labels": [NEEDS_HUMAN_LABEL]})
}

pub(crate) fn print_proposals(out: &mut impl Write, values: &[Value]) -> Result<()> {
    for value in values {
        serde_json::to_writer(&mut *out, value)?;
        writeln!(out)?;
    }
    eprintln!("Proposed only; not applied. Re-run with --apply to write with your token.");
    Ok(())
}

/// The ask cannot be proposed onto the board before it has a number; apply adds it.
fn create_proposals(
    blocked: &ItemRef,
    repository: &str,
    args: &Create,
    title: &str,
    body: &str,
) -> Vec<Value> {
    vec![
        json!({"type": "create_issue", "target_repo": repository, "title": title, "body": body, "labels": [ASK_LABEL, NEEDS_HUMAN_LABEL], "assignees": [args.assignee]}),
        needs_human("add_labels", blocked),
    ]
}

/// `answer` is `None` for an ask already closed, which is not closed again.
fn resolve_proposals(ask: &ItemRef, answer: Option<&str>, unblock: Option<&ItemRef>) -> Vec<Value> {
    let close = answer.map(|answer| json!({"type": "close_issue", "target_repo": ask.repository, "issue_number": ask.number, "body": answer}));
    close
        .into_iter()
        .chain(unblock.map(|item| needs_human("remove_labels", item)))
        .collect()
}

/// GitHub's hosts and repository names are case-insensitive, and the parser keeps
/// their spelling.
fn same_issue(a: &ItemRef, b: &ItemRef) -> bool {
    a.number == b.number
        && a.host.eq_ignore_ascii_case(&b.host)
        && a.repository.eq_ignore_ascii_case(&b.repository)
}

/// The item an ask's first line names, as `compose` writes it.
fn blocks(body: &str, host: &str) -> Option<ItemRef> {
    let url = body
        .lines()
        .next()?
        .strip_prefix(BLOCKS)?
        .trim()
        .trim_matches('`');
    board_forge::parse_github_item(url, None, host).ok()
}

fn on_board<'a>(snapshot: &'a Snapshot, identity: &ItemRef) -> Result<&'a board_core::Item> {
    let item = snapshot
        .items
        .iter()
        .find(|item| item.identity == *identity)
        .with_context(|| {
            format!(
                "{} is not on the board in this snapshot; add it with item add",
                identity.display()
            )
        })?;
    ensure!(
        item.content_kind == "issue",
        "{} is a {}, not an issue",
        identity.display(),
        item.content_kind
    );
    Ok(item)
}

/// Validate against a saved snapshot and print the proposals; never writes.
pub fn create_preview(
    snapshot: &Snapshot,
    blocked: &ItemRef,
    repository: &str,
    args: &Create,
    (title, body): &(String, String),
    out: &mut impl Write,
) -> Result<()> {
    board_forge::validate_github_project(&snapshot.project)?;
    on_board(snapshot, blocked)?;
    print_proposals(
        out,
        &create_proposals(blocked, repository, args, title, body),
    )
}

/// Check the blocked item's membership and both labels, then print the proposals or,
/// with `apply`, open the ask labelled, add it to the board and label the blocked item.
/// The issue is only opened once everything else checks out; a failure after that
/// leaves the rows printed before it done.
pub fn create<T: Transport>(
    github: &GitHub<T>,
    blocked: &ItemRef,
    repository: &str,
    args: &Create,
    (title, body): &(String, String),
    out: &mut impl Write,
) -> Result<()> {
    let (project, _, _) = github.schema()?;
    let (blocked_issue, blocked_item) = github.issue_item(&project, blocked)?;
    ensure!(
        blocked_item.is_some(),
        "{} is not on the board; add it with item add",
        blocked.display()
    );
    let target =
        github.issue_target(repository, &[ASK_LABEL, NEEDS_HUMAN_LABEL], &args.assignee)?;
    let label = github.label_id(&blocked.repository, NEEDS_HUMAN_LABEL)?;
    if !args.apply {
        return print_proposals(
            out,
            &create_proposals(blocked, repository, args, title, body),
        );
    }
    let (issue, number) = github.create_issue(&target, title, body)?;
    let ask = ItemRef {
        host: blocked.host.clone(),
        repository: repository.into(),
        number,
    };
    writeln!(out, "{}\tcreated", board_forge::github_item_url(&ask)?)?;
    github
        .add_item(&project, &issue)
        .context("add the ask; rows above were done")?;
    writeln!(out, "{}\tadded", ask.display())?;
    github
        .set_label(&blocked.repository, &blocked_issue, &label, true)
        .context("label the blocked item; rows above were done")?;
    writeln!(out, "{}\t{NEEDS_HUMAN_LABEL}\tlabelled", blocked.display())?;
    Ok(())
}

pub fn check_answer(answer: &str) -> Result<()> {
    ensure!(!answer.trim().is_empty(), "the answer is empty");
    Ok(())
}

/// Validate against a saved snapshot and print the proposals; never writes.
pub fn resolve_preview(
    snapshot: &Snapshot,
    ask: &ItemRef,
    answer: &str,
    out: &mut impl Write,
) -> Result<()> {
    check_answer(answer)?;
    board_forge::validate_github_project(&snapshot.project)?;
    let item = on_board(snapshot, ask)?;
    ensure!(
        item.labels.iter().any(|l| l == ASK_LABEL),
        "{} is not labelled {ASK_LABEL}",
        ask.display()
    );
    ensure!(item.state == "open", "{} is already closed", ask.display());
    // A snapshot holds neither the ask's body nor the other asks.
    eprintln!(
        "Whether {NEEDS_HUMAN_LABEL} comes off the blocked item is read from the live forge; preview without --snapshot to see it."
    );
    print_proposals(out, &resolve_proposals(ask, Some(answer), None))
}

/// The item the ask blocks, its node ID and the `needs-human` label's, if the label
/// should come off it: it is on the board, carries the label and no other open ask on
/// the board names it. Where that cannot be read whole, the label stays, said on
/// stderr.
fn unblock<T: Transport>(
    github: &GitHub<T>,
    project: &str,
    ask: &ItemRef,
    body: &str,
) -> Result<Option<(ItemRef, String, String)>> {
    let Some(blocked) = blocks(body, &ask.host) else {
        eprintln!(
            "{} names no item on a {BLOCKS:?} first line; no label removed",
            ask.display()
        );
        return Ok(None);
    };
    // The body is editable: an item off the board is not this command's to unlabel.
    if github.issue_item(project, &blocked)?.1.is_none() {
        eprintln!(
            "{} is not on the board; {NEEDS_HUMAN_LABEL} not removed",
            blocked.display()
        );
        return Ok(None);
    }
    let facts = github.issue_facts(&blocked)?;
    if !facts
        .labels
        .iter()
        .any(|l| l.eq_ignore_ascii_case(NEEDS_HUMAN_LABEL))
    {
        return Ok(None);
    }
    let (asks, complete) = github.open_board_issues(project, ASK_LABEL)?;
    if !complete {
        eprintln!(
            "The board's open asks could not all be read; {NEEDS_HUMAN_LABEL} stays on {}",
            blocked.display()
        );
        return Ok(None);
    }
    let others: Vec<_> = asks
        .iter()
        .filter(|(other, body)| {
            !same_issue(other, ask)
                && blocks(body, &ask.host).is_some_and(|item| same_issue(&item, &blocked))
        })
        .map(|(other, _)| other.display())
        .collect();
    if !others.is_empty() {
        eprintln!(
            "{} stays {NEEDS_HUMAN_LABEL}: still blocked by {}",
            blocked.display(),
            others.join(", ")
        );
        return Ok(None);
    }
    let label = github.label_id(&blocked.repository, NEEDS_HUMAN_LABEL)?;
    Ok(Some((blocked, facts.id, label)))
}

/// Check that the issue is an ask on the board, then print the proposals or, with
/// `apply`, comment the answer, close it as completed and take `needs-human` off the
/// item it blocked ([`unblock`]). An ask already closed is neither commented nor
/// closed again, so a rerun after a failed unlabel only finishes it; one after a
/// failed close posts the answer twice.
pub fn resolve<T: Transport>(
    github: &GitHub<T>,
    ask: &ItemRef,
    answer: &str,
    apply: bool,
    out: &mut impl Write,
) -> Result<()> {
    check_answer(answer)?;
    let (project, _, _) = github.schema()?;
    let facts = github.issue_facts(ask)?;
    ensure!(
        facts.labels.iter().any(|l| l == ASK_LABEL),
        "{} is not labelled {ASK_LABEL}",
        ask.display()
    );
    let (_, item) = github.issue_item(&project, ask)?;
    ensure!(
        item.is_some(),
        "{} is not on the board; add it with item add",
        ask.display()
    );
    if !facts.open {
        eprintln!(
            "{} is already closed; only {NEEDS_HUMAN_LABEL} is taken off the item it blocks",
            ask.display()
        );
    }
    let unblock = unblock(github, &project, ask, &facts.body)?;
    if !apply {
        return print_proposals(
            out,
            &resolve_proposals(
                ask,
                facts.open.then_some(answer),
                unblock.as_ref().map(|(item, _, _)| item),
            ),
        );
    }
    if facts.open {
        let comment = github.comment(&ask.repository, &facts.id, answer)?;
        writeln!(out, "{comment}\tcommented")?;
        github
            .close_completed(&ask.repository, &facts.id)
            .context("close the ask; rows above were done")?;
        writeln!(out, "{}\tclosed", ask.display())?;
    }
    if let Some((blocked, issue, label)) = unblock {
        github
            .set_label(&blocked.repository, &issue, &label, false)
            .context("unlabel the blocked item; rows above were done")?;
        writeln!(
            out,
            "{}\t{NEEDS_HUMAN_LABEL}\tunlabelled",
            blocked.display()
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::cell::RefCell;

    #[derive(Parser)]
    struct Cmd {
        #[command(flatten)]
        create: Create,
    }

    fn create_args(args: &[&str]) -> Create {
        Cmd::try_parse_from(std::iter::once("ask").chain(args.iter().copied()))
            .unwrap()
            .create
    }

    fn blocked() -> ItemRef {
        board_forge::parse_github_item("example/intake#7", None, "github.com").unwrap()
    }

    #[test]
    fn compose_table() {
        let long = "é".repeat(150);
        for (args, repository, title, body) in [
            (
                vec![
                    "Which target?\n  Pick one.",
                    "--option",
                    " Stable ",
                    "--option",
                    "Both",
                    "--recommend",
                    "it is supported",
                    "--context",
                    "We ship twice a year.",
                ],
                "example/intake",
                "Which target? Pick one.".to_owned(),
                "Blocks: https://github.com/example/intake/issues/7\n\nWe ship twice a year.\n\nQ: Which target?\n  Pick one.\n\nOptions:\nA) Stable\nB) Both\n\nRecommended: A, because it is supported\n\nReply with a letter or free text. Only comments by @op count.\n",
            ),
            // An ask elsewhere names the blocked item without mentioning it there.
            (
                vec![&long, "--title", "Short"],
                "example/tracker",
                "Short".to_owned(),
                &*format!(
                    "Blocks: `https://github.com/example/intake/issues/7`\n\nQ: {long}\n\nReply with your answer in free text. Only comments by @op count.\n"
                ),
            ),
        ] {
            let args = create_args(&[&args[..], &["--item", "7", "--assignee", "op"]].concat());
            assert_eq!(
                compose(&args, &blocked(), repository).unwrap(),
                (title, body.to_owned())
            );
        }
        let default = create_args(&[&long, "--item", "7", "--assignee", "op"]);
        assert_eq!(
            compose(&default, &blocked(), "example/intake").unwrap().0,
            "é".repeat(TITLE_CHARS)
        );
        let options: Vec<String> = (0..11)
            .flat_map(|i| ["--option".into(), format!("o{i}")])
            .collect();
        let eleven: Vec<&str> = options.iter().map(String::as_str).collect();
        for (extra, error) in [
            (vec!["--option", "only"], "no options or 2 to 10"),
            (eleven, "no options or 2 to 10"),
            (vec!["--recommend", "why"], "--recommend needs options"),
            (
                vec!["--option", "a", "--option", " "],
                "option B must be one non-empty line",
            ),
            (
                vec!["--option", "a", "--option", "b\nRecommended: B"],
                "option B must be one",
            ),
            (
                vec!["--option", "a", "--option", "b", "--recommend", " "],
                "needs a reason",
            ),
            (vec!["--title", "two\nlines"], "one line"),
            (vec!["--title", " "], "title is empty"),
        ] {
            let args =
                create_args(&[&["Q?", "--item", "7", "--assignee", "op"][..], &extra].concat());
            let message = format!(
                "{:#}",
                compose(&args, &blocked(), "example/intake").unwrap_err()
            );
            assert!(message.contains(error), "{extra:?}: {message}");
        }
        for (question, assignee, repository, error) in [
            (" \n", "op", "example/intake", "question is empty"),
            ("Q?", "a;id", "example/intake", "invalid GitHub login"),
            ("Q?", "op", "example", "invalid ask repository"),
            ("Q?", "op", "a/b/c", "invalid ask repository"),
        ] {
            let args = create_args(&[question, "--item", "7", "--assignee", assignee]);
            let message = format!("{:#}", compose(&args, &blocked(), repository).unwrap_err());
            assert!(message.contains(error), "{question:?}: {message}");
        }
    }

    const ASK_BODY: &str = "Blocks: https://github.com/example/intake/issues/7\n\nQ: Q?\n";

    /// Issues #7 (blocked, on the board) and #8 (the ask, once opened or for resolve).
    struct Board {
        /// Issue numbers in example/intake with an item on the board.
        members: RefCell<Vec<u64>>,
        /// Labels the repository has; GitHub looks them up case-insensitively.
        labels: &'static [&'static str],
        /// Logins GitHub lists as assignable for the queried login.
        assignable: &'static [&'static str],
        /// This mutation fails.
        fail: Option<&'static str>,
        /// GitHub silently drops the assignee.
        drop_assignee: bool,
        ask_labels: &'static [&'static str],
        ask_body: &'static str,
        ask_open: bool,
        labels_truncated: bool,
        blocked_labels: &'static [&'static str],
        /// Open asks on the board, by number and body, one to a page after #7's.
        asks: &'static [(u64, &'static str)],
        /// The board also holds an item whose content cannot be seen.
        redacted: bool,
        mutations: RefCell<Vec<String>>,
    }

    impl Board {
        fn new() -> Self {
            Self {
                members: RefCell::new(vec![7]),
                labels: &[ASK_LABEL, NEEDS_HUMAN_LABEL],
                assignable: &["op-bot", "OP"],
                fail: None,
                drop_assignee: false,
                ask_labels: &["bug", ASK_LABEL, NEEDS_HUMAN_LABEL],
                ask_body: ASK_BODY,
                ask_open: true,
                labels_truncated: false,
                blocked_labels: &[NEEDS_HUMAN_LABEL],
                // The ask itself, and one that blocks another item.
                asks: &[
                    (8, ASK_BODY),
                    (9, "Blocks: https://github.com/example/intake/issues/70\n"),
                ],
                redacted: false,
                mutations: RefCell::new(vec![]),
            }
        }
    }

    impl Transport for Board {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            if query.contains("repositoryOwner(login:") {
                return Ok(
                    json!({"repositoryOwner":{"projectV2":{"id":"P","url":"https://github.com/orgs/example/projects/1"}}}),
                );
            }
            if query.contains("fields(first") {
                return Ok(json!({"node":{"fields":{"nodes":[
                    {"id":"F_status","name":"Status","dataType":"SINGLE_SELECT","options":[{"id":"o-Todo","name":"Todo"}]}
                ],"pageInfo":{"hasNextPage":false}}}}));
            }
            if query.contains("items(first:") {
                assert_eq!(variables["id"], "P");
                let page = variables["cursor"]
                    .as_str()
                    .map_or(0, |c| c.parse::<usize>().unwrap());
                let issue = |number: u64, labels: &[&str], body: &str| {
                    let labels: Vec<_> = labels.iter().map(|l| json!({"name": l})).collect();
                    json!({"content": {"__typename": "Issue", "number": number, "state": "OPEN", "body": body,
                        "repository": {"nameWithOwner": "example/intake"},
                        "labels": {"nodes": labels, "pageInfo": {"hasNextPage": false}}}})
                };
                let nodes = match page.checked_sub(1) {
                    None => {
                        let mut nodes = vec![
                            issue(7, self.blocked_labels, ""),
                            json!({"content": {"__typename": "DraftIssue"}}),
                        ];
                        if self.redacted {
                            nodes.push(json!({"content": null}));
                        }
                        nodes
                    }
                    Some(i) => vec![issue(self.asks[i].0, &[ASK_LABEL], self.asks[i].1)],
                };
                return Ok(json!({"node": {"items": {"nodes": nodes, "pageInfo": {
                    "hasNextPage": page < self.asks.len(), "endCursor": (page + 1).to_string()}}}}));
            }
            let repo = format!(
                "{}/{}",
                variables["owner"].as_str().unwrap_or_default(),
                variables["repo"].as_str().unwrap_or_default()
            );
            if query.contains("issue(number:") {
                let number = variables["number"].as_u64().unwrap();
                let nodes: Vec<_> = self
                    .members
                    .borrow()
                    .iter()
                    .filter(|n| **n == number && repo == "example/intake")
                    .map(|n| json!({"id": format!("M{n}"), "project": {"id": "P"}}))
                    .collect();
                let (labels, body) = if number == 8 {
                    (self.ask_labels, self.ask_body)
                } else {
                    (self.blocked_labels, "")
                };
                let labels: Vec<_> = labels.iter().map(|l| json!({"name": l})).collect();
                return Ok(json!({"repository":{"issue":{
                    "id": format!("I{number}"), "number": number,
                    "state": if number != 8 || self.ask_open { "OPEN" } else { "CLOSED" },
                    "body": body,
                    "repository": {"nameWithOwner": repo},
                    "projectItems": {"nodes": nodes, "pageInfo": {"hasNextPage": false}},
                    "labels": {"nodes": labels, "pageInfo": {"hasNextPage": number == 8 && self.labels_truncated}}
                }}}));
            }
            if query.starts_with("query") && query.contains("label(name:") {
                let wanted = variables["label"].as_str().unwrap();
                let label = self
                    .labels
                    .iter()
                    .find(|l| l.eq_ignore_ascii_case(wanted))
                    .map(|name| json!({"id": format!("L-{wanted}"), "name": name}));
                return Ok(json!({"repository": {"nameWithOwner": repo, "label": label}}));
            }
            if query.starts_with("query") && query.contains("assignableUsers") {
                assert_eq!(variables["login"], "op");
                let users: Vec<_> = self
                    .assignable
                    .iter()
                    .map(|login| json!({"id": format!("U-{login}"), "login": login}))
                    .collect();
                return Ok(json!({
                    "repository": {"id": "R", "nameWithOwner": repo,
                        "assignableUsers": {"nodes": users}}
                }));
            }
            let name = query[query.find('{').unwrap() + 1..]
                .split('(')
                .next()
                .unwrap();
            self.mutations.borrow_mut().push(name.into());
            if self.fail == Some(name) {
                anyhow::bail!("{name} failed");
            }
            match name {
                "createIssue" => {
                    assert_eq!(
                        (&variables["repo"], &variables["labels"], &variables["user"]),
                        (
                            &json!("R"),
                            &json!(["L-question", "L-needs-human"]),
                            &json!("U-OP")
                        )
                    );
                    let assignees: &[_] = if self.drop_assignee {
                        &[]
                    } else {
                        &[json!({"login":"op"})]
                    };
                    // GitHub may list the labels in any order.
                    Ok(
                        json!({"createIssue":{"issue":{"id":"I8","number":8,"url":"u",
                        "labels":{"nodes":[{"name":NEEDS_HUMAN_LABEL},{"name":ASK_LABEL}]},"assignees":{"nodes":assignees}}}}),
                    )
                }
                "addLabelsToLabelable" | "removeLabelsFromLabelable" => {
                    assert_eq!(
                        (&variables["id"], &variables["label"]),
                        (&json!("I7"), &json!("L-needs-human"))
                    );
                    Ok(json!({name: {"labelable": {"id": "I7"}}}))
                }
                "addProjectV2ItemById" => {
                    assert_eq!(variables["issue"], "I8");
                    self.members.borrow_mut().push(8);
                    Ok(json!({"addProjectV2ItemById":{"item":{"id":"M8"}}}))
                }
                "addComment" => {
                    assert_eq!(variables["id"], "I8");
                    Ok(
                        json!({"addComment":{"commentEdge":{"node":{"url":"https://github.com/example/intake/issues/8#issuecomment-1"}}}}),
                    )
                }
                "closeIssue" => {
                    Ok(json!({"closeIssue":{"issue":{"id":variables["id"],"state":"CLOSED"}}}))
                }
                _ => anyhow::bail!("unexpected query {query}"),
            }
        }
    }

    fn github(board: Board) -> GitHub<Board> {
        GitHub {
            transport: board,
            owner: "example".into(),
            number: 1,
        }
    }

    type Setup = fn(&mut Board);
    const NONE: Setup = |_| {};

    #[test]
    fn create_table() {
        let preview = "{\"assignees\":[\"op\"],\"body\":\"Blocks: https://github.com/example/intake/issues/7\\n\\nQ: Q?\\n\\nOptions:\\nA) a\\nB) b\\n\\nReply with a letter or free text. Only comments by @op count.\\n\",\"labels\":[\"question\",\"needs-human\"],\"target_repo\":\"example/intake\",\"title\":\"Q?\",\"type\":\"create_issue\"}\n{\"item_number\":7,\"labels\":[\"needs-human\"],\"target_repo\":\"example/intake\",\"type\":\"add_labels\"}\n";
        // Columns: board setup, apply, stdout, mutations in order, error.
        // No Status is written: the pass derives it from the labels.
        for (setup, apply, stdout, mutations, error) in [
            (
                NONE,
                true,
                "https://github.com/example/intake/issues/8\tcreated\nexample/intake#8\tadded\nexample/intake#7\tneeds-human\tlabelled\n",
                vec![
                    "createIssue",
                    "addProjectV2ItemById",
                    "addLabelsToLabelable",
                ],
                None,
            ),
            (NONE, false, preview, vec![], None),
            // Every refusal below precedes opening the issue.
            (
                |b| b.members.borrow_mut().clear(),
                true,
                "",
                vec![],
                Some("example/intake#7 is not on the board"),
            ),
            (
                |b| b.labels = &[NEEDS_HUMAN_LABEL],
                true,
                "",
                vec![],
                Some("has no \"question\" label; create it first"),
            ),
            (
                |b| b.labels = &["Question", NEEDS_HUMAN_LABEL],
                true,
                "",
                vec![],
                Some("has no \"question\" label"),
            ),
            (
                |b| b.labels = &[ASK_LABEL],
                true,
                "",
                vec![],
                Some("has no \"needs-human\" label; create it first"),
            ),
            (
                |b| b.labels = &[ASK_LABEL],
                false,
                "",
                vec![],
                Some("has no \"needs-human\" label"),
            ),
            (
                |b| b.assignable = &[],
                true,
                "",
                vec![],
                Some("op cannot be assigned issues in example/intake"),
            ),
            // GitHub matches by prefix; a longer login is someone else.
            (
                |b| b.assignable = &["op-bot"],
                true,
                "",
                vec![],
                Some("op cannot be assigned"),
            ),
            // A preview checks what apply would, so it does not promise an ask that fails.
            (
                |b| b.labels = &[NEEDS_HUMAN_LABEL],
                false,
                "",
                vec![],
                Some("has no \"question\" label"),
            ),
            (
                |b| b.fail = Some("addProjectV2ItemById"),
                true,
                "https://github.com/example/intake/issues/8\tcreated\n",
                vec!["createIssue", "addProjectV2ItemById"],
                Some("add the ask; rows above were done"),
            ),
            (
                |b| b.fail = Some("addLabelsToLabelable"),
                true,
                "https://github.com/example/intake/issues/8\tcreated\nexample/intake#8\tadded\n",
                vec![
                    "createIssue",
                    "addProjectV2ItemById",
                    "addLabelsToLabelable",
                ],
                Some("label the blocked item; rows above were done"),
            ),
            // A dropped assignee is reported, naming the issue that now exists.
            (
                |b| b.drop_assignee = true,
                true,
                "",
                vec!["createIssue"],
                Some("opened example/intake#8, but GitHub did not apply"),
            ),
        ] {
            let mut board = Board::new();
            setup(&mut board);
            let github = github(board);
            let mut args = create_args(&[
                "Q?",
                "--item",
                "7",
                "--assignee",
                "op",
                "--option",
                "a",
                "--option",
                "b",
            ]);
            args.apply = apply;
            let mut out = Vec::new();
            let composed = compose(&args, &blocked(), "example/intake").unwrap();
            let result = create(
                &github,
                &blocked(),
                "example/intake",
                &args,
                &composed,
                &mut out,
            );
            assert_eq!(String::from_utf8(out).unwrap(), stdout, "{error:?}");
            assert_eq!(*github.transport.mutations.borrow(), mutations, "{error:?}");
            match error {
                Some(error) => assert!(
                    format!("{:#}", result.unwrap_err()).contains(error),
                    "{error}"
                ),
                None => result.unwrap(),
            }
        }
    }

    #[test]
    fn resolve_table() {
        let close = "{\"body\":\"Use A.\",\"issue_number\":8,\"target_repo\":\"example/intake\",\"type\":\"close_issue\"}\n";
        let preview = &*format!(
            "{close}{{\"item_number\":7,\"labels\":[\"needs-human\"],\"target_repo\":\"example/intake\",\"type\":\"remove_labels\"}}\n"
        );
        let closed = "https://github.com/example/intake/issues/8#issuecomment-1\tcommented\nexample/intake#8\tclosed\n";
        let on_board: Setup = |b| b.members.borrow_mut().push(8);
        // Columns: board setup, answer, apply, stdout, mutations in order, error.
        // No Status is written: the pass derives Done from the closed ask.
        for (setup, answer, apply, stdout, mutations, error) in [
            (
                on_board,
                "Use A.",
                true,
                &*format!("{closed}example/intake#7\tneeds-human\tunlabelled\n"),
                vec!["addComment", "closeIssue", "removeLabelsFromLabelable"],
                None,
            ),
            (on_board, "Use A.", false, preview, vec![], None),
            // The label stays while another open ask on the board names the item, on
            // any page and in any spelling of its repository (search could miss an
            // ask just opened), ...
            (
                |b| {
                    b.members.borrow_mut().push(8);
                    b.asks = &[
                        (8, ASK_BODY),
                        (10, "Blocks: `https://github.com/example/intake/issues/7`\n"),
                    ]
                },
                "Use A.",
                true,
                closed,
                vec!["addComment", "closeIssue"],
                None,
            ),
            (
                |b| {
                    b.members.borrow_mut().push(8);
                    b.asks = &[(10, "Blocks: https://github.com/Example/Intake/issues/7\n")]
                },
                "Use A.",
                true,
                closed,
                vec!["addComment", "closeIssue"],
                None,
            ),
            (
                |b| {
                    b.members.borrow_mut().push(8);
                    b.asks = &[(10, "Blocks: https://github.com/example/intake/issues/7\n")]
                },
                "Use A.",
                false,
                close,
                vec![],
                None,
            ),
            // ... when the board cannot all be read, ...
            (
                |b| {
                    b.members.borrow_mut().push(8);
                    b.redacted = true
                },
                "Use A.",
                true,
                closed,
                vec!["addComment", "closeIssue"],
                None,
            ),
            // ... when the edited ask names an item off the board, or the item left it, ...
            (
                |b| {
                    b.members.borrow_mut().push(8);
                    b.ask_body = "Blocks: https://github.com/other/repo/issues/7\n\nQ: Q?\n"
                },
                "Use A.",
                true,
                closed,
                vec!["addComment", "closeIssue"],
                None,
            ),
            (
                |b| *b.members.borrow_mut() = vec![8],
                "Use A.",
                true,
                closed,
                vec!["addComment", "closeIssue"],
                None,
            ),
            // ... and the ask names no item; one not labelled has nothing to remove.
            (
                |b| {
                    b.members.borrow_mut().push(8);
                    b.ask_body = "Q: Q?\n"
                },
                "Use A.",
                true,
                closed,
                vec!["addComment", "closeIssue"],
                None,
            ),
            (
                |b| {
                    b.members.borrow_mut().push(8);
                    b.blocked_labels = &["bug"]
                },
                "Use A.",
                true,
                closed,
                vec!["addComment", "closeIssue"],
                None,
            ),
            (
                |b| {
                    b.members.borrow_mut().push(8);
                    b.fail = Some("removeLabelsFromLabelable")
                },
                "Use A.",
                true,
                closed,
                vec!["addComment", "closeIssue", "removeLabelsFromLabelable"],
                Some("unlabel the blocked item; rows above were done"),
            ),
            (
                |b| {
                    b.members.borrow_mut().push(8);
                    b.fail = Some("closeIssue")
                },
                "Use A.",
                true,
                "https://github.com/example/intake/issues/8#issuecomment-1\tcommented\n",
                vec!["addComment", "closeIssue"],
                Some("close the ask; rows above were done"),
            ),
            // Every refusal below precedes the comment.
            (
                on_board,
                " \n",
                true,
                "",
                vec![],
                Some("the answer is empty"),
            ),
            (
                |b| {
                    b.members.borrow_mut().push(8);
                    b.ask_labels = &["bug"]
                },
                "A",
                true,
                "",
                vec![],
                Some("is not labelled question"),
            ),
            (
                |b| {
                    b.members.borrow_mut().push(8);
                    b.ask_labels = &["Question"]
                },
                "A",
                true,
                "",
                vec![],
                Some("is not labelled question"),
            ),
            // A rerun finds the ask closed and posts nothing twice: after a failed
            // unlabel it only finishes that, and after success it does nothing.
            (
                |b| {
                    b.members.borrow_mut().push(8);
                    b.ask_open = false
                },
                "A",
                true,
                "example/intake#7\tneeds-human\tunlabelled\n",
                vec!["removeLabelsFromLabelable"],
                None,
            ),
            (
                |b| {
                    b.members.borrow_mut().push(8);
                    b.ask_open = false
                },
                "A",
                false,
                "{\"item_number\":7,\"labels\":[\"needs-human\"],\"target_repo\":\"example/intake\",\"type\":\"remove_labels\"}\n",
                vec![],
                None,
            ),
            (
                |b| {
                    b.members.borrow_mut().push(8);
                    b.ask_open = false;
                    b.blocked_labels = &["bug"]
                },
                "A",
                true,
                "",
                vec![],
                None,
            ),
            (
                |b| {
                    b.members.borrow_mut().push(8);
                    b.labels_truncated = true
                },
                "A",
                true,
                "",
                vec![],
                Some("more than 100 labels"),
            ),
            (
                NONE,
                "A",
                true,
                "",
                vec![],
                Some("example/intake#8 is not on the board"),
            ),
            (
                |b| {
                    b.members.borrow_mut().push(8);
                    b.labels = &[ASK_LABEL]
                },
                "A",
                true,
                "",
                vec![],
                Some("has no \"needs-human\" label"),
            ),
        ] {
            let mut board = Board::new();
            setup(&mut board);
            let github = github(board);
            let ask =
                board_forge::parse_github_item("example/intake#8", None, "github.com").unwrap();
            let mut out = Vec::new();
            let result = resolve(&github, &ask, answer, apply, &mut out);
            assert_eq!(String::from_utf8(out).unwrap(), stdout, "{error:?}");
            assert_eq!(*github.transport.mutations.borrow(), mutations, "{error:?}");
            match error {
                Some(error) => assert!(
                    format!("{:#}", result.unwrap_err()).contains(error),
                    "{error}"
                ),
                None => result.unwrap(),
            }
        }
    }
}
