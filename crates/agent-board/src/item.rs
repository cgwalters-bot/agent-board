//! `item add` and `item edit`. Both print a validated `update_project` proposal unless
//! `--apply` is given; only then do they write, with the operator's own token, as
//! `project init` does. No workflow here passes `--apply`, and an agent holds no token.
use crate::ask::print_proposals;
use anyhow::{Context, Result, ensure};
use board_core::{ItemRef, Snapshot};
use board_forge::github::{DEFAULT_HOST, FieldWrite, GitHub, Transport, field_writes};
use clap::Args;
use serde_json::{Value, json};
use std::io::Write;

#[derive(Args)]
pub struct Add {
    /// Issue URL, OWNER/REPO#N, or N with --repo.
    pub item: String,
    /// Priority option to set, for example P1.
    #[arg(long)]
    pub priority: Option<String>,
    /// Turn option; defaults to Coordinator when the board offers it.
    #[arg(long)]
    pub turn: Option<String>,
    /// Write to the live board with your token; without it, only print the proposal.
    #[arg(long)]
    pub apply: bool,
}

#[derive(Args)]
pub struct Edit {
    /// Issue URL, OWNER/REPO#N, or N with --repo.
    pub item: String,
    /// Set a single-select or text field, for example Status=Todo (repeatable).
    #[arg(long = "field", value_name = "NAME=VALUE", required = true)]
    pub fields: Vec<String>,
    /// Write to the live board with your token; without it, only print the proposal.
    #[arg(long)]
    pub apply: bool,
}

impl Add {
    pub fn edits(&self) -> Vec<String> {
        self.priority
            .iter()
            .map(|priority| format!("Priority={priority}"))
            .chain(self.turn.iter().map(|turn| format!("Turn={turn}")))
            .collect()
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Add,
    Edit,
}

fn add_defaults(fields: &[Value], edits: &[String], mode: Mode) -> Vec<String> {
    let mut edits = edits.to_vec();
    if mode == Mode::Add
        && !edits.iter().any(|edit| edit.starts_with("Turn="))
        && fields.iter().any(|field| {
            field["name"] == "Turn"
                && field["dataType"] == "SINGLE_SELECT"
                && field["options"].as_array().is_some_and(|options| {
                    options.iter().any(|option| option["name"] == "Coordinator")
                })
        })
    {
        edits.push("Turn=Coordinator".into());
    }
    edits
}

/// Parse an issue to write. The proposal names neither host nor content type, so an
/// identity on another host or an explicit pull request URL is refused here rather than
/// lowered as a github.com issue.
pub fn issue_identity(input: &str, repository: Option<&str>, host: &str) -> Result<ItemRef> {
    let identity = board_forge::github::parse_github_issue(input, repository, host)?;
    ensure!(
        identity.host == DEFAULT_HOST,
        "unsupported item host {}",
        identity.host
    );
    Ok(identity)
}

fn proposal(project: &str, item: &ItemRef, writes: &[FieldWrite]) -> Value {
    let fields: serde_json::Map<_, _> = writes
        .iter()
        .map(|w| (w.name.clone(), w.value.clone().into()))
        .collect();
    json!({"type": "update_project", "project": project, "content_type": "issue", "content_number": item.number, "target_repo": item.repository, "fields": fields})
}

/// Validate against a saved snapshot and print the proposal; never writes.
pub fn preview(
    snapshot: &Snapshot,
    identity: &ItemRef,
    edits: &[String],
    mode: Mode,
    out: &mut impl Write,
) -> Result<()> {
    board_forge::validate_github_project(&snapshot.project)?;
    let writes = field_writes(
        &snapshot.fields,
        &add_defaults(&snapshot.fields, edits, mode),
    )?;
    let item = snapshot
        .items
        .iter()
        .find(|item| item.identity == *identity);
    ensure!(
        mode == Mode::Add || item.is_some(),
        "item not on the board in this snapshot; add it with item add"
    );
    if let Some(item) = item {
        ensure!(
            item.content_kind == "issue",
            "only issues can be added or edited, not {}",
            item.content_kind
        );
    }
    print_proposals(
        out,
        &[proposal(snapshot.project.as_str(), identity, &writes)],
    )
}

/// Validate against the live schema and membership, then print the proposal or, with
/// `apply`, add the item if needed and set each field in order. `added` is set only
/// when this call adds the item, as soon as GitHub returns its ID, so a caller can
/// remove what it created after a later failure. An existing membership, even one
/// project automation made a moment ago, never authorizes removal. A failure partway
/// leaves the rows printed before it done.
pub fn write<T: Transport>(
    github: &GitHub<T>,
    identity: &ItemRef,
    edits: &[String],
    mode: Mode,
    apply: bool,
    added: &mut Option<String>,
    out: &mut impl Write,
) -> Result<()> {
    let (project, url, fields) = github.schema()?;
    // Every edit is resolved before membership is read or anything is written.
    let writes = field_writes(&fields, &add_defaults(&fields, edits, mode))?;
    let (issue, member) = github.issue_item(&project, identity)?;
    ensure!(
        mode == Mode::Add || member.is_some(),
        "{} is not on the board; add it with item add",
        identity.display()
    );
    if !apply {
        return print_proposals(out, &[proposal(url.as_str(), identity, &writes)]);
    }
    let (item, action) = match member {
        Some(item) => (item, "present"),
        None => {
            let item = github.add_item(&project, &issue)?;
            *added = Some(item.clone());
            (item, "added")
        }
    };
    writeln!(out, "{}\t{action}", identity.display())?;
    for write in &writes {
        github
            .set_field(&project, &item, write)
            .with_context(|| format!("set {}; rows above were done", write.name))?;
        writeln!(out, "{}\t{}\tset", write.name, write.value)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A board whose membership and field values change with each mutation.
    struct Board {
        members: RefCell<Vec<String>>,
        values: RefCell<Vec<(String, Value)>>,
        mutations: RefCell<Vec<String>>,
        /// The URL GitHub reports for the requested project.
        url: &'static str,
        /// Answer the issue lookup with this repository instead of the requested one.
        lookup_repo: Option<&'static str>,
        /// The issue's other project memberships span another page.
        truncated: bool,
        /// Report a different value than was written.
        lie: bool,
    }

    impl Board {
        fn new() -> Self {
            Self {
                members: RefCell::new(vec![]),
                values: RefCell::new(vec![]),
                mutations: RefCell::new(vec![]),
                url: "https://github.com/orgs/example/projects/1",
                lookup_repo: None,
                truncated: false,
                lie: false,
            }
        }
    }

    impl Transport for Board {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            if query.contains("repositoryOwner(login:") {
                return Ok(json!({"repositoryOwner":{"projectV2":{"id":"P","url":self.url}}}));
            }
            if query.contains("fields(first") {
                return Ok(json!({"node":{"fields":{"nodes":[
                    {"id":"F_status","name":"Status","dataType":"SINGLE_SELECT","options":[{"id":"todo","name":"Todo"},{"id":"done","name":"Done"}]},
                    {"id":"F_priority","name":"Priority","dataType":"SINGLE_SELECT","options":[{"id":"p1","name":"P1"},{"id":"p2","name":"P2"}]},
                    {"id":"F_why","name":"Why","dataType":"TEXT"},
                    {"id":"F_size","name":"Size","dataType":"NUMBER"},
                    {"id":"F_dup","name":"Org","dataType":"TEXT"},
                    {"id":"F_dup2","name":"Org","dataType":"TEXT"}
                ],"pageInfo":{"hasNextPage":false}}}}));
            }
            if query.contains("issue(number:") {
                assert_eq!(variables["number"], 7);
                let nodes: Vec<_> =
                    std::iter::once(json!({"id":"M_other","project":{"id":"P_other"}}))
                        .chain(
                            self.members
                                .borrow()
                                .iter()
                                .map(|id| json!({"id":id,"project":{"id":"P"}})),
                        )
                        .collect();
                let repo = self.lookup_repo.map_or_else(
                    || {
                        format!(
                            "{}/{}",
                            variables["owner"].as_str().unwrap(),
                            variables["repo"].as_str().unwrap()
                        )
                    },
                    str::to_owned,
                );
                return Ok(
                    json!({"repository":{"issue":{"id":"I","number":7,"repository":{"nameWithOwner":repo},"projectItems":{"nodes":nodes,"pageInfo":{"hasNextPage":self.truncated}}}}}),
                );
            }
            self.mutations.borrow_mut().push(query.into());
            if query.contains("addProjectV2ItemById(") {
                assert_eq!(variables["project"], "P");
                assert_eq!(variables["issue"], "I");
                self.members.borrow_mut().push("M".into());
                return Ok(json!({"addProjectV2ItemById":{"item":{"id":"M"}}}));
            }
            if query.contains("updateProjectV2ItemFieldValue(") {
                assert_eq!(variables["project"], "P");
                assert_eq!(variables["item"], "M");
                let mut value = variables["value"].clone();
                if self.lie {
                    value = json!({"optionId":"other","text":"other"});
                } else if let Some(option) = value.get("singleSelectOptionId").cloned() {
                    value = json!({"optionId": option});
                }
                self.values.borrow_mut().push((
                    variables["field"].as_str().unwrap().into(),
                    variables["value"].clone(),
                ));
                return Ok(
                    json!({"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":"M","fieldValueByName":value}}}),
                );
            }
            anyhow::bail!("unexpected query {query}")
        }
    }

    #[test]
    fn write_table() {
        let edits = |e: &[&str]| e.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Columns: board setup, mode, apply, edits, already a member, rows printed,
        // fields written (in order), error.
        type Setup = fn(&mut Board);
        let none: Setup = |_| {};
        for (setup, mode, apply, fields, member, rows, written, error) in [
            (
                none,
                Mode::Add,
                true,
                edits(&["Priority=P1"]),
                false,
                "example/intake#7\tadded\nPriority\tP1\tset\n",
                vec!["F_priority"],
                None,
            ),
            (
                none,
                Mode::Add,
                true,
                edits(&[]),
                false,
                "example/intake#7\tadded\n",
                vec![],
                None,
            ),
            // Adding a member is not a second add: it only sets the fields.
            (
                none,
                Mode::Add,
                true,
                edits(&["Priority=P2"]),
                true,
                "example/intake#7\tpresent\nPriority\tP2\tset\n",
                vec!["F_priority"],
                None,
            ),
            (
                none,
                Mode::Edit,
                true,
                edits(&["Status=Done", "Why=a=b"]),
                true,
                "example/intake#7\tpresent\nStatus\tDone\tset\nWhy\ta=b\tset\n",
                vec!["F_status", "F_why"],
                None,
            ),
            // Without --apply nothing is written; the proposal is printed instead.
            (
                none,
                Mode::Edit,
                false,
                edits(&["Status=Done"]),
                true,
                "{\"content_number\":7,\"content_type\":\"issue\",\"fields\":{\"Status\":\"Done\"},\"project\":\"https://github.com/orgs/example/projects/1\",\"target_repo\":\"example/intake\",\"type\":\"update_project\"}\n",
                vec![],
                None,
            ),
            (
                none,
                Mode::Add,
                false,
                edits(&[]),
                false,
                "{\"content_number\":7,\"content_type\":\"issue\",\"fields\":{},\"project\":\"https://github.com/orgs/example/projects/1\",\"target_repo\":\"example/intake\",\"type\":\"update_project\"}\n",
                vec![],
                None,
            ),
            // Every refusal below precedes any write.
            (
                none,
                Mode::Edit,
                true,
                edits(&["Status=Done"]),
                false,
                "",
                vec![],
                Some("not on the board"),
            ),
            (
                none,
                Mode::Edit,
                true,
                edits(&["Status=Doing"]),
                true,
                "",
                vec![],
                Some("Status has no unambiguous option \"Doing\"; options: Todo, Done"),
            ),
            (
                none,
                Mode::Edit,
                true,
                edits(&["Status=done"]),
                true,
                "",
                vec![],
                Some("no unambiguous option"),
            ),
            (
                none,
                Mode::Edit,
                true,
                edits(&["Lead=me"]),
                true,
                "",
                vec![],
                Some("board has no Lead field"),
            ),
            (
                none,
                Mode::Edit,
                true,
                edits(&["Org=x"]),
                true,
                "",
                vec![],
                Some("more than one Org field"),
            ),
            (
                none,
                Mode::Edit,
                true,
                edits(&["Size=3"]),
                true,
                "",
                vec![],
                Some("Size is a NUMBER field"),
            ),
            (
                none,
                Mode::Edit,
                true,
                edits(&["Why="]),
                true,
                "",
                vec![],
                Some("clearing a field is not supported"),
            ),
            (
                none,
                Mode::Edit,
                true,
                edits(&["Status"]),
                true,
                "",
                vec![],
                Some("expected NAME=VALUE"),
            ),
            (
                none,
                Mode::Edit,
                true,
                edits(&["=Done"]),
                true,
                "",
                vec![],
                Some("expected NAME=VALUE"),
            ),
            (
                none,
                Mode::Edit,
                true,
                edits(&["Status=Todo", "Status=Done"]),
                true,
                "",
                vec![],
                Some("Status is set more than once"),
            ),
            (
                |b| b.url = "https://github.com/orgs/other/projects/1",
                Mode::Add,
                true,
                edits(&[]),
                false,
                "",
                vec![],
                Some("not example/1"),
            ),
            (
                |b| b.lookup_repo = Some("example/other"),
                Mode::Add,
                true,
                edits(&[]),
                false,
                "",
                vec![],
                Some("different issue"),
            ),
            // Unseen memberships could hide this board's item: neither add nor edit.
            (
                |b| b.truncated = true,
                Mode::Add,
                true,
                edits(&[]),
                false,
                "",
                vec![],
                Some("membership unknown"),
            ),
            // A write GitHub does not report back fails after the add was done.
            (
                |b| b.lie = true,
                Mode::Add,
                true,
                edits(&["Priority=P1"]),
                false,
                "example/intake#7\tadded\n",
                vec!["F_priority"],
                Some("set Priority; rows above were done: Priority update receipt mismatch"),
            ),
            // A later failure on an existing member reports no added item to remove.
            (
                |b| b.lie = true,
                Mode::Add,
                true,
                edits(&["Priority=P1"]),
                true,
                "example/intake#7\tpresent\n",
                vec!["F_priority"],
                Some("set Priority; rows above were done: Priority update receipt mismatch"),
            ),
        ] {
            let mut board = Board::new();
            setup(&mut board);
            if member {
                board.members.borrow_mut().push("M".into());
            }
            let github = GitHub {
                transport: board,
                owner: "example".into(),
                number: 1,
            };
            let identity =
                board_forge::parse_github_item("example/intake#7", None, "github.com").unwrap();
            let mut out = Vec::new();
            let mut added = None;
            let result = write(
                &github, &identity, &fields, mode, apply, &mut added, &mut out,
            );
            let label = format!("{fields:?} {apply}");
            assert_eq!(String::from_utf8(out).unwrap(), rows, "{label}");
            let fields_written: Vec<_> = github
                .transport
                .values
                .borrow()
                .iter()
                .map(|(f, _)| f.clone())
                .collect();
            assert_eq!(fields_written, written, "{label}");
            let was_added = rows.contains("\tadded\n");
            assert_eq!(added.is_some(), was_added, "{label}");
            assert_eq!(
                github
                    .transport
                    .mutations
                    .borrow()
                    .iter()
                    .any(|q| q.contains("addProjectV2ItemById(")),
                was_added,
                "{label}"
            );
            match error {
                Some(error) => assert!(
                    format!("{:#}", result.unwrap_err()).contains(error),
                    "{label}"
                ),
                None => result.unwrap(),
            }
            if !apply || error.is_some_and(|e| !e.contains("rows above")) {
                assert!(github.transport.mutations.borrow().is_empty(), "{label}");
            }
        }
    }

    #[test]
    fn add_turn_defaults_table() {
        let fields = [
            json!({"name":"Turn","dataType":"SINGLE_SELECT","options":[{"name":"Coordinator"},{"name":"Worker"}]}),
        ];
        for (schema, mode, edits, expected) in [
            (&fields[..], Mode::Add, vec![], vec!["Turn=Coordinator"]),
            (
                &fields[..],
                Mode::Add,
                vec!["Priority=P1"],
                vec!["Priority=P1", "Turn=Coordinator"],
            ),
            (
                &fields[..],
                Mode::Add,
                vec!["Turn=Worker"],
                vec!["Turn=Worker"],
            ),
            (&fields[..], Mode::Edit, vec![], vec![]),
            (&[][..], Mode::Add, vec![], vec![]),
        ] {
            let edits: Vec<_> = edits.iter().map(|s| s.to_string()).collect();
            assert_eq!(add_defaults(schema, &edits, mode), expected);
        }
    }
}
