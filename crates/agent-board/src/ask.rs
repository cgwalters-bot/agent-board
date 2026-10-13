//! `ask create`, `ask list` and `ask resolve`: a question for the operator is an issue
//! labelled `question` and assigned to them, on the board as Needs human together with
//! the item it blocks; resolving comments the answer, closes it and sets it Done. This
//! follows homegit's `bot-board question` and `resolve`. Like `item add`/`item edit`,
//! both print proposals unless `--apply` is given, and only then write with the
//! operator's own token. No workflow here passes `--apply`, and an agent holds no token.
use anyhow::{Context, Result, ensure};
use board_core::{ItemRef, Snapshot};
use board_forge::github::{FieldWrite, GitHub, Transport, field_writes};
use clap::Args;
use serde_json::{Value, json};
use std::io::Write;

/// The label that marks an issue as an ask; it must already exist in the repository.
pub const ASK_LABEL: &str = "question";
/// The Status both an open ask and the item it blocks carry.
pub const ASK_STATUS: &str = "Needs human";
const RESOLVED_STATUS: &str = "Done";
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
    /// Open the issue and write to the live board with your token; without it, only print proposals.
    #[arg(long)]
    pub apply: bool,
}

#[derive(Args)]
pub struct Resolve {
    /// The ask: URL, OWNER/REPO#N, or N with --repo.
    pub item: String,
    /// The answer, posted as a comment before the ask is closed.
    pub answer: String,
    /// Comment, close and write to the live board with your token; without it, only print proposals.
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
        format!("Blocks: {url}\n\n")
    } else {
        format!("Blocks: `{url}`\n\n")
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

fn status(project: &str, item: &ItemRef, value: &str) -> Value {
    json!({"type": "update_project", "project": project, "content_type": "issue", "content_number": item.number, "target_repo": item.repository, "fields": {"Status": value}})
}

fn print_proposals(out: &mut impl Write, values: &[Value]) -> Result<()> {
    for value in values {
        serde_json::to_writer(&mut *out, value)?;
        writeln!(out)?;
    }
    eprintln!("Proposed only; not applied. Re-run with --apply to write with your token.");
    Ok(())
}

/// Resolve a Status write. `field_writes` also accepts a text field, but the snapshot
/// reader requires Status to be single-select and marks items incomplete otherwise; so
/// a text Status is refused here, before anything is written or proposed.
fn status_write(fields: &[Value], value: &str) -> Result<Vec<FieldWrite>> {
    ensure!(
        fields
            .iter()
            .filter(|f| f["name"] == "Status")
            .all(|f| f["dataType"] == "SINGLE_SELECT"),
        "the board's Status field is not single-select"
    );
    field_writes(fields, &[format!("Status={value}")])
}

/// The ask cannot be proposed onto the board before it has a number; apply adds it.
fn create_proposals(
    project: &str,
    blocked: &ItemRef,
    repository: &str,
    args: &Create,
    title: &str,
    body: &str,
) -> Vec<Value> {
    vec![
        json!({"type": "create_issue", "target_repo": repository, "title": title, "body": body, "labels": [ASK_LABEL], "assignees": [args.assignee]}),
        status(project, blocked, ASK_STATUS),
    ]
}

fn resolve_proposals(project: &str, ask: &ItemRef, answer: &str) -> Vec<Value> {
    vec![
        json!({"type": "close_issue", "target_repo": ask.repository, "issue_number": ask.number, "body": answer}),
        status(project, ask, RESOLVED_STATUS),
    ]
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
    status_write(&snapshot.fields, ASK_STATUS)?;
    on_board(snapshot, blocked)?;
    print_proposals(
        out,
        &create_proposals(
            snapshot.project.as_str(),
            blocked,
            repository,
            args,
            title,
            body,
        ),
    )
}

/// Validate the schema and the blocked item's membership, then print the proposals or,
/// with `apply`, open the ask, add it and set both items Needs human. The issue is only
/// opened once everything else checks out; a failure after that leaves the rows printed
/// before it done.
pub fn create<T: Transport>(
    github: &GitHub<T>,
    blocked: &ItemRef,
    repository: &str,
    args: &Create,
    (title, body): &(String, String),
    out: &mut impl Write,
) -> Result<()> {
    let (project, url, fields) = github.schema()?;
    let writes = status_write(&fields, ASK_STATUS)?;
    let (_, blocked_item) = github.issue_item(&project, blocked)?;
    let blocked_item = blocked_item.with_context(|| {
        format!(
            "{} is not on the board; add it with item add",
            blocked.display()
        )
    })?;
    let target = github.issue_target(repository, ASK_LABEL, &args.assignee)?;
    if !args.apply {
        return print_proposals(
            out,
            &create_proposals(url.as_str(), blocked, repository, args, title, body),
        );
    }
    let (issue, number) = github.create_issue(&target, title, body)?;
    let ask = ItemRef {
        host: blocked.host.clone(),
        repository: repository.into(),
        number,
    };
    writeln!(out, "{}\tcreated", board_forge::github_item_url(&ask)?)?;
    let item = github
        .add_item(&project, &issue)
        .context("add the ask; rows above were done")?;
    writeln!(out, "{}\tadded", ask.display())?;
    for (identity, item) in [(&ask, &item), (blocked, &blocked_item)] {
        for write in &writes {
            github
                .set_field(&project, item, write)
                .with_context(|| format!("set {}; rows above were done", write.name))?;
            writeln!(
                out,
                "{}\t{}\t{}\tset",
                identity.display(),
                write.name,
                write.value
            )?;
        }
    }
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
    status_write(&snapshot.fields, RESOLVED_STATUS)?;
    let item = on_board(snapshot, ask)?;
    ensure!(
        item.labels.iter().any(|l| l == ASK_LABEL),
        "{} is not labelled {ASK_LABEL}",
        ask.display()
    );
    ensure!(item.state == "open", "{} is already closed", ask.display());
    print_proposals(
        out,
        &resolve_proposals(snapshot.project.as_str(), ask, answer),
    )
}

/// Check that the issue is an open ask on the board, then print the proposals or, with
/// `apply`, comment the answer, close it as completed and set it Done. Only an open ask
/// is resolved, so a rerun after success does not post the answer twice; one after a
/// failed close does. As in homegit, the item it blocked keeps its Status: what happens
/// next depends on the answer.
pub fn resolve<T: Transport>(
    github: &GitHub<T>,
    ask: &ItemRef,
    answer: &str,
    apply: bool,
    out: &mut impl Write,
) -> Result<()> {
    check_answer(answer)?;
    let (project, url, fields) = github.schema()?;
    let writes = status_write(&fields, RESOLVED_STATUS)?;
    let (issue, open, labels) = github.issue_labels(ask)?;
    ensure!(
        labels.iter().any(|l| l == ASK_LABEL),
        "{} is not labelled {ASK_LABEL}",
        ask.display()
    );
    ensure!(open, "{} is already closed", ask.display());
    let (_, item) = github.issue_item(&project, ask)?;
    let item = item.with_context(|| {
        format!(
            "{} is not on the board; add it with item add",
            ask.display()
        )
    })?;
    if !apply {
        return print_proposals(out, &resolve_proposals(url.as_str(), ask, answer));
    }
    let comment = github.comment(&ask.repository, &issue, answer)?;
    writeln!(out, "{comment}\tcommented")?;
    github
        .close_completed(&ask.repository, &issue)
        .context("close the ask; rows above were done")?;
    writeln!(out, "{}\tclosed", ask.display())?;
    for write in &writes {
        github
            .set_field(&project, &item, write)
            .with_context(|| format!("set {}; rows above were done", write.name))?;
        writeln!(
            out,
            "{}\t{}\t{}\tset",
            ask.display(),
            write.name,
            write.value
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

    /// Issues #7 (blocked, on the board) and #8 (the ask, once opened or for resolve).
    struct Board {
        status_type: &'static str,
        statuses: &'static [&'static str],
        /// Issue numbers with an item on the board.
        members: RefCell<Vec<u64>>,
        /// Name GitHub returns for the requested label, if it exists.
        label: Option<&'static str>,
        /// Logins GitHub lists as assignable for the queried login.
        assignable: &'static [&'static str],
        /// This mutation fails.
        fail: Option<&'static str>,
        /// GitHub silently drops the assignee.
        drop_assignee: bool,
        ask_labels: &'static [&'static str],
        ask_open: bool,
        labels_truncated: bool,
        mutations: RefCell<Vec<String>>,
    }

    impl Board {
        fn new() -> Self {
            Self {
                status_type: "SINGLE_SELECT",
                statuses: &["Todo", "Needs human", "Done"],
                members: RefCell::new(vec![7]),
                label: Some(ASK_LABEL),
                assignable: &["op-bot", "OP"],
                fail: None,
                drop_assignee: false,
                ask_labels: &["bug", ASK_LABEL],
                ask_open: true,
                labels_truncated: false,
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
                let options: Vec<_> = self
                    .statuses
                    .iter()
                    .map(|name| json!({"id": format!("o-{name}"), "name": name}))
                    .collect();
                return Ok(json!({"node":{"fields":{"nodes":[
                    {"id":"F_status","name":"Status","dataType":self.status_type,"options":options}
                ],"pageInfo":{"hasNextPage":false}}}}));
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
                    .filter(|n| **n == number)
                    .map(|n| json!({"id": format!("M{n}"), "project": {"id": "P"}}))
                    .collect();
                let labels: Vec<_> = self.ask_labels.iter().map(|l| json!({"name": l})).collect();
                return Ok(json!({"repository":{"issue":{
                    "id": format!("I{number}"), "number": number,
                    "state": if self.ask_open { "OPEN" } else { "CLOSED" },
                    "repository": {"nameWithOwner": repo},
                    "projectItems": {"nodes": nodes, "pageInfo": {"hasNextPage": false}},
                    "labels": {"nodes": labels, "pageInfo": {"hasNextPage": self.labels_truncated}}
                }}}));
            }
            if query.starts_with("query") && query.contains("label(name:") {
                assert_eq!(
                    (&variables["label"], &variables["login"]),
                    (&json!(ASK_LABEL), &json!("op"))
                );
                let users: Vec<_> = self
                    .assignable
                    .iter()
                    .map(|login| json!({"id": format!("U-{login}"), "login": login}))
                    .collect();
                return Ok(json!({
                    "repository": {"id": "R", "nameWithOwner": repo,
                        "label": self.label.map(|name| json!({"id": "L", "name": name})),
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
                        (&variables["repo"], &variables["label"], &variables["user"]),
                        (&json!("R"), &json!("L"), &json!("U-OP"))
                    );
                    let assignees: &[_] = if self.drop_assignee {
                        &[]
                    } else {
                        &[json!({"login":"op"})]
                    };
                    Ok(
                        json!({"createIssue":{"issue":{"id":"I8","number":8,"url":"u",
                        "labels":{"nodes":[{"name":ASK_LABEL}]},"assignees":{"nodes":assignees}}}}),
                    )
                }
                "addProjectV2ItemById" => {
                    assert_eq!(variables["issue"], "I8");
                    self.members.borrow_mut().push(8);
                    Ok(json!({"addProjectV2ItemById":{"item":{"id":"M8"}}}))
                }
                "updateProjectV2ItemFieldValue" => {
                    let value = json!({"optionId": variables["value"]["singleSelectOptionId"]});
                    Ok(
                        json!({"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":variables["item"],"fieldValueByName":value}}}),
                    )
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
        let preview = "{\"assignees\":[\"op\"],\"body\":\"Blocks: https://github.com/example/intake/issues/7\\n\\nQ: Q?\\n\\nOptions:\\nA) a\\nB) b\\n\\nReply with a letter or free text. Only comments by @op count.\\n\",\"labels\":[\"question\"],\"target_repo\":\"example/intake\",\"title\":\"Q?\",\"type\":\"create_issue\"}\n{\"content_number\":7,\"content_type\":\"issue\",\"fields\":{\"Status\":\"Needs human\"},\"project\":\"https://github.com/orgs/example/projects/1\",\"target_repo\":\"example/intake\",\"type\":\"update_project\"}\n";
        // Columns: board setup, apply, stdout, mutations in order, error.
        for (setup, apply, stdout, mutations, error) in [
            (
                NONE,
                true,
                "https://github.com/example/intake/issues/8\tcreated\nexample/intake#8\tadded\nexample/intake#8\tStatus\tNeeds human\tset\nexample/intake#7\tStatus\tNeeds human\tset\n",
                vec![
                    "createIssue",
                    "addProjectV2ItemById",
                    "updateProjectV2ItemFieldValue",
                    "updateProjectV2ItemFieldValue",
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
                |b| b.statuses = &["Todo", "Done"],
                true,
                "",
                vec![],
                Some("Status has no unambiguous option \"Needs human\""),
            ),
            // A text Status would take the value, but snapshots then mark items incomplete.
            (
                |b| b.status_type = "TEXT",
                true,
                "",
                vec![],
                Some("Status field is not single-select"),
            ),
            (
                |b| b.status_type = "TEXT",
                false,
                "",
                vec![],
                Some("Status field is not single-select"),
            ),
            (
                |b| b.label = None,
                true,
                "",
                vec![],
                Some("has no \"question\" label; create it first"),
            ),
            (
                |b| b.label = Some("Question"),
                true,
                "",
                vec![],
                Some("has no \"question\" label"),
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
                |b| b.label = None,
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
        let preview = "{\"body\":\"Use A.\",\"issue_number\":8,\"target_repo\":\"example/intake\",\"type\":\"close_issue\"}\n{\"content_number\":8,\"content_type\":\"issue\",\"fields\":{\"Status\":\"Done\"},\"project\":\"https://github.com/orgs/example/projects/1\",\"target_repo\":\"example/intake\",\"type\":\"update_project\"}\n";
        let on_board: Setup = |b| b.members.borrow_mut().push(8);
        for (setup, answer, apply, stdout, mutations, error) in [
            (
                on_board,
                "Use A.",
                true,
                "https://github.com/example/intake/issues/8#issuecomment-1\tcommented\nexample/intake#8\tclosed\nexample/intake#8\tStatus\tDone\tset\n",
                vec!["addComment", "closeIssue", "updateProjectV2ItemFieldValue"],
                None,
            ),
            (on_board, "Use A.", false, preview, vec![], None),
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
            // A rerun after success finds the ask closed and posts nothing twice.
            (
                |b| {
                    b.members.borrow_mut().push(8);
                    b.ask_open = false
                },
                "A",
                true,
                "",
                vec![],
                Some("already closed"),
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
                    b.statuses = &["Todo"]
                },
                "A",
                true,
                "",
                vec![],
                Some("Status has no unambiguous option \"Done\""),
            ),
            (
                |b| {
                    b.members.borrow_mut().push(8);
                    b.status_type = "TEXT"
                },
                "A",
                true,
                "",
                vec![],
                Some("Status field is not single-select"),
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

    #[test]
    fn previews_refuse_text_status() {
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        snapshot.fields = vec![json!({"name":"Status","dataType":"TEXT"})];
        let ask = board_forge::parse_github_item("example/intake#2", None, "github.com").unwrap();
        let args = create_args(&["Q?", "--item", "1", "--assignee", "op"]);
        let composed = compose(&args, &blocked(), "example/intake").unwrap();
        let mut out = Vec::new();
        for result in [
            create_preview(
                &snapshot,
                &blocked(),
                "example/intake",
                &args,
                &composed,
                &mut out,
            ),
            resolve_preview(&snapshot, &ask, "A", &mut out),
        ] {
            assert!(
                format!("{:#}", result.unwrap_err()).contains("Status field is not single-select")
            );
        }
        assert!(out.is_empty());
    }
}
