//! One operator-editable issue; the baseline is local, never hidden in its body.
use anyhow::{Context, Result, ensure};
use board_forge::github::{Client, Transport};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Write, path::PathBuf};

// Bound untrusted issue/history text and parsing work, not authority.
const BODY_LIMIT: usize = 60_000;
const HISTORY_LIMIT: usize = 100;

#[derive(Args)]
pub struct Settings {
    /// Caller-owned agent-board.json from a trusted default-branch checkout.
    #[arg(long, default_value = "agent-board.json")]
    pub config: PathBuf,
}

#[derive(Args)]
pub struct Add {
    #[command(flatten)]
    pub settings: Settings,
    /// Question shown in the decision heading.
    pub question: String,
    /// Background paragraph, without headings or fences.
    #[arg(long)]
    pub context: String,
    /// Alternative answer (repeat 2 to 10 times).
    #[arg(long = "option", required = true)]
    pub options: Vec<String>,
    /// Reason to recommend the first option (A).
    #[arg(long)]
    pub recommend: String,
    /// Blocked issue, OWNER/REPO#N.
    #[arg(long)]
    pub item: String,
    /// Priority of the blocked decision.
    #[arg(long, value_parser = ["P0", "P1", "P2"])]
    pub priority: String,
    /// Update the issue with your token; otherwise only print the merged body.
    #[arg(long)]
    pub apply: bool,
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Debug)]
#[serde(deny_unknown_fields)]
struct Target {
    repository: String,
    issue: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct State {
    target: Target,
    body: String,
    high_water: u64,
}

pub enum Operation<'a> {
    Add(&'a Add),
    Read,
    Resolve(&'a str, &'a str),
}

pub fn cli_check(snapshot: bool, host: &str, json: bool) -> Result<()> {
    ensure!(
        !snapshot,
        "decisions use an issue transport, not --snapshot"
    );
    ensure!(host == "github.com", "decisions support only github.com");
    ensure!(!json, "decisions emit JSON directly; --json is unsupported");
    Ok(())
}

pub fn run(settings: &Settings, operation: Operation<'_>, apply: bool) -> Result<()> {
    let config: Value = serde_json::from_slice(
        &std::fs::read(&settings.config).context("read decisions configuration")?,
    )?;
    let target: Target = serde_json::from_value(
        config
            .get("decisions")
            .context("decisions are not configured")?
            .clone(),
    )?;
    board_forge::poll::validate_repository(&target.repository)?;
    ensure!(
        (1..=i32::MAX as u64).contains(&target.issue),
        "decisions issue must be a positive GraphQL Int"
    );
    let path = settings.config.with_extension("decisions-state.md");
    let state = match std::fs::read_to_string(&path) {
        Ok(text) => {
            let state: State = serde_json::from_str(
                text.strip_prefix("```json\n")
                    .and_then(|s| s.strip_suffix("\n```\n"))
                    .context("invalid fenced decisions state")?,
            )?;
            ensure!(state.target == target, "decisions state target mismatch");
            Some(state)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).context("read decisions state"),
    };
    let result = execute(
        &Client::from_env()?,
        &target,
        state.as_ref().map(|s| s.body.as_str()),
        state.as_ref().map_or(0, |s| s.high_water),
        operation,
        apply,
    )?;
    if let Some(body) = result.written {
        let text = format!(
            "```json\n{}\n```\n",
            serde_json::to_string_pretty(&State {
                target,
                body,
                high_water: result.high_water
            })?
        );
        save_state(&path, &text)?;
    }
    writeln!(
        std::io::stdout().lock(),
        "{}",
        serde_json::to_string_pretty(&result.output)?
    )?;
    Ok(())
}

fn save_state(path: &std::path::Path, text: &str) -> Result<()> {
    // Threat: private issue text leaking to other local users under umask 022,
    // or a preexisting temporary symlink redirecting that text. Create a fresh,
    // owner-only file, then rename within the directory for atomic publication.
    let temporary = path.with_extension("md.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .context("create private decisions baseline after issue update")?;
    let result = file
        .write_all(text.as_bytes())
        .context("write decisions baseline after issue update")
        .and_then(|()| {
            std::fs::rename(&temporary, path)
                .context("publish decisions baseline after issue update")
        });
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

struct ResultBody {
    output: Value,
    written: Option<String>,
    high_water: u64,
}

struct Issue {
    id: String,
    body: String,
    updated: String,
    baseline: Option<String>,
    high_water: u64,
    history_complete: bool,
}

fn fetch<T: Transport>(transport: &T, target: &Target, recover: bool) -> Result<Issue> {
    let (owner, name) = target
        .repository
        .split_once('/')
        .context("invalid repository")?;
    let value = transport.graphql(
        "query Decisions($owner:String!,$name:String!,$number:Int!,$limit:Int!){ viewer{login} repository(owner:$owner,name:$name){issue(number:$number){id body updatedAt userContentEdits(first:$limit){nodes{editor{login} editedAt diff} pageInfo{hasNextPage}}}}}",
        json!({"owner":owner,"name":name,"number":target.issue,"limit":HISTORY_LIMIT}),
    )?;
    let issue = value
        .pointer("/repository/issue")
        .context("decisions issue missing")?;
    let body = issue["body"]
        .as_str()
        .context("missing decisions body")?
        .to_owned();
    ensure!(
        body.len() <= BODY_LIMIT,
        "decisions body exceeds {BODY_LIMIT} bytes"
    );
    let viewer = value
        .pointer("/viewer/login")
        .and_then(Value::as_str)
        .context("missing viewer")?;
    let edits = issue
        .pointer("/userContentEdits/nodes")
        .and_then(Value::as_array)
        .context("missing edit history")?;
    // GitHub serves edits newest-first for both first: and last:, and an
    // edit's diff is the whole body as that edit left it. Reading the order
    // backwards takes an older bot write (or the current body) as the
    // baseline and silently drops the operator's later edits, so refuse any
    // history that is not newest-first instead of trusting the order.
    let mut previous: Option<&str> = None;
    for edit in edits {
        let at = edit["editedAt"].as_str().context("missing edit time")?;
        ensure!(
            previous.is_none_or(|newer| at <= newer),
            "edit history is not newest-first"
        );
        previous = Some(at);
    }
    // The newest bot edit holds our last write; a null (redacted) diff
    // leaves no baseline, which fails closed below.
    let baseline = edits
        .iter()
        .find(|e| e.pointer("/editor/login").and_then(Value::as_str) == Some(viewer))
        .and_then(|e| e["diff"].as_str())
        .filter(|s| s.len() <= BODY_LIMIT)
        .map(str::to_owned);
    let mut high_water = entries(&body)?.keys().next_back().copied().unwrap_or(0);
    if recover {
        for edit in edits {
            if let Some(version) = edit["diff"].as_str() {
                ensure!(
                    version.len() <= BODY_LIMIT,
                    "historical decisions body exceeds {BODY_LIMIT} bytes"
                );
                // Old revisions may contain an operator's temporarily malformed
                // document. Conservatively count every valid heading ID rather
                // than requiring old structure to remain valid forever.
                for line in version.lines() {
                    if let Some(rest) = line.strip_prefix("### D")
                        && let Some((id, _)) = rest.split_once(' ')
                        && let Ok(id) = decision_id(&format!("D{id}"))
                    {
                        high_water = high_water.max(id);
                    }
                }
            }
        }
    }
    Ok(Issue {
        id: issue["id"].as_str().context("missing issue id")?.into(),
        updated: issue["updatedAt"]
            .as_str()
            .context("missing issue timestamp")?
            .into(),
        body,
        baseline,
        high_water,
        history_complete: issue
            .pointer("/userContentEdits/pageInfo/hasNextPage")
            .and_then(Value::as_bool)
            == Some(false)
            && edits.iter().all(|e| e["diff"].is_string()),
    })
}

// Byte spans let resolution move operator text without normalization or reflow.
#[derive(Clone)]
struct Entry {
    start: usize,
    end: usize,
    done: bool,
}

fn entries(body: &str) -> Result<BTreeMap<u64, Entry>> {
    ensure!(
        body.len() <= BODY_LIMIT,
        "decisions body exceeds {BODY_LIMIT} bytes"
    );
    let mut entries = BTreeMap::new();
    let mut offset = 0;
    let mut previous = None;
    let mut done = false;
    let mut fence: Option<char> = None;
    let mut done_count = 0;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let c = trimmed.chars().next();
            if fence == c {
                fence = None;
            } else if fence.is_none() {
                fence = c;
            }
        }
        if fence.is_none() && trimmed == "## Done" {
            done_count += 1;
            ensure!(done_count == 1, "duplicate Done section");
            if let Some(id) = previous.take() {
                let entry: &mut Entry = entries.get_mut(&id).context("missing entry")?;
                entry.end = offset;
            }
            done = true;
        }
        if fence.is_none()
            && let Some(rest) = trimmed.strip_prefix("### D")
        {
            let (id, title) = rest.split_once(' ').context("invalid decision heading")?;
            let id = decision_id(&format!("D{id}"))?;
            ensure!(!title.trim().is_empty(), "empty decision heading");
            if let Some(previous) = previous {
                let entry: &mut Entry = entries.get_mut(&previous).context("missing entry")?;
                entry.end = offset;
            }
            ensure!(!entries.contains_key(&id), "duplicate decision D{id}");
            entries.insert(
                id,
                Entry {
                    start: offset,
                    end: body.len(),
                    done,
                },
            );
            previous = Some(id);
        }
        offset += line.len();
    }
    ensure!(
        fence.is_none(),
        "unclosed Markdown fence in decisions document"
    );
    Ok(entries)
}

fn decision_id(id: &str) -> Result<u64> {
    let number = id.strip_prefix('D').context("expected D<n>")?;
    let n: u64 = number.parse().context("expected D<n>")?;
    ensure!(
        n > 0 && number == n.to_string(),
        "expected canonical positive D<n>"
    );
    Ok(n)
}

fn single_line(text: &str) -> Result<()> {
    ensure!(
        !text.trim().is_empty() && !text.contains(['\r', '\n']),
        "expected nonempty single-line text"
    );
    Ok(())
}

fn edit(
    body: &str,
    baseline: Option<&str>,
    high_water: u64,
    operation: &Operation<'_>,
) -> Result<String> {
    let current = entries(body)?;
    match operation {
        Operation::Add(args) => {
            for text in [&args.question, &args.recommend, &args.item, &args.priority] {
                single_line(text)?;
            }
            ensure!(
                ["P0", "P1", "P2"].contains(&args.priority.as_str()),
                "invalid priority"
            );
            board_forge::parse_github_item(&args.item, None, "github.com")?;
            ensure!(
                (2..=10).contains(&args.options.len()),
                "give 2 to 10 options"
            );
            for option in &args.options {
                single_line(option)?;
            }
            // Context cannot inject a new document section or swallow later headings.
            ensure!(!args.context.trim().is_empty(), "empty context");
            ensure!(
                !args
                    .context
                    .lines()
                    .any(|l| l.starts_with('#') || l.starts_with("```") || l.starts_with("~~~")),
                "context must not contain headings or fences"
            );
            let old = baseline.map(entries).transpose()?.unwrap_or_default();
            let max = current
                .keys()
                .chain(old.keys())
                .max()
                .copied()
                .unwrap_or(0)
                .max(high_water);
            let id = max.checked_add(1).context("decision ids exhausted")?;
            let mut entry = format!(
                "### D{id} {} ({}, blocks {})\n{}\n",
                args.question, args.priority, args.item, args.context
            );
            for (i, option) in args.options.iter().enumerate() {
                entry.push_str(&format!("- [ ] {}) {option}", char::from(b'A' + i as u8)));
                if i == 0 {
                    entry.push_str(&format!(" ← recommended: {}", args.recommend));
                }
                entry.push('\n');
            }
            entry.push_str("\nReply:\n\n");
            let at = done_offset(body);
            Ok(format!(
                "{}{}{}{}",
                &body[..at],
                if at > 0 && !body[..at].ends_with("\n\n") {
                    "\n\n"
                } else {
                    ""
                },
                entry,
                &body[at..]
            ))
        }
        Operation::Resolve(id, answer) => {
            let id = decision_id(id)?;
            single_line(answer)?;
            let entry = current.get(&id).context("decision not found")?;
            ensure!(!entry.done, "decision already Done");
            let mut next = format!("{}{}", &body[..entry.start], &body[entry.end..]);
            if !next.ends_with('\n') {
                next.push('\n');
            }
            if done_offset(&next) == next.len() {
                next.push_str("\n## Done\n\n");
            }
            next.push_str(&body[entry.start..entry.end]);
            if !next.ends_with('\n') {
                next.push('\n');
            }
            next.push_str(&format!("\nOutcome: {answer}\n\n"));
            Ok(next)
        }
        Operation::Read => anyhow::bail!("read is not a mutation"),
    }
}

fn done_offset(body: &str) -> usize {
    // entries() has already checked structure; ignore literal headings in fences.
    let mut fence = None;
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        if line.starts_with("```") || line.starts_with("~~~") {
            let c = line.chars().next();
            if fence == c {
                fence = None;
            } else if fence.is_none() {
                fence = c;
            }
        }
        if fence.is_none() && line.trim_end_matches(['\r', '\n']) == "## Done" {
            return offset;
        }
        offset += line.len();
    }
    body.len()
}

fn changes(before: &str, after: &str) -> Result<Value> {
    let old = entries(before)?;
    let new = entries(after)?;
    let mut changes = Vec::new();
    for id in old
        .keys()
        .chain(new.keys())
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
    {
        let previous = old.get(&id).map(|e| &before[e.start..e.end]);
        let current = new.get(&id).map(|e| &after[e.start..e.end]);
        if previous == current {
            continue;
        }
        let text = current.unwrap_or("");
        changes.push(json!({
            "id": format!("D{id}"),
            "ticked": text.lines().filter(|l| l.starts_with("- [x] ") || l.starts_with("- [X] ")).collect::<Vec<_>>(),
            "reply": text.split_once("Reply:").map(|(_, reply)| reply),
            "before": previous, "after": current,
        }));
    }
    Ok(json!({"changes":changes, "other_edits": before != after && changes.is_empty()}))
}

fn execute<T: Transport>(
    transport: &T,
    target: &Target,
    baseline: Option<&str>,
    high_water: u64,
    operation: Operation<'_>,
    apply: bool,
) -> Result<ResultBody> {
    let issue = fetch(transport, target, baseline.is_none())?;
    let local_baseline = baseline.is_some();
    let baseline = baseline.or(issue.baseline.as_deref());
    if matches!(operation, Operation::Read) {
        let baseline = baseline.context(
            "no bot baseline or bot edit in the newest 100 edits; cannot attribute changes",
        )?;
        return Ok(ResultBody {
            output: changes(baseline, &issue.body)?,
            written: None,
            high_water,
        });
    }
    // Without a baseline an empty issue can be initialized, but an existing
    // document must not silently lose its high-water ID after state loss.
    ensure!(
        baseline.is_some() || issue.body.trim().is_empty(),
        "no baseline for existing decisions document"
    );
    ensure!(
        local_baseline || issue.history_complete,
        "edit history is incomplete; restore the local baseline before writing"
    );
    let next = edit(
        &issue.body,
        baseline,
        high_water.max(issue.high_water),
        &operation,
    )?;
    let high_water = high_water
        .max(issue.high_water)
        .max(entries(&next)?.keys().next_back().copied().unwrap_or(0));
    ensure!(
        next.len() <= BODY_LIMIT,
        "updated decisions body exceeds {BODY_LIMIT} bytes"
    );
    entries(&next)?;
    if apply {
        // Threat: overwrite an operator edit made during our read/merge. GitHub
        // has no conditional updateIssue; this detects edits before the final
        // read, not the unavoidable read-to-mutation race. No retries.
        let latest = fetch(transport, target, false)?;
        ensure!(
            latest.id == issue.id && latest.body == issue.body && latest.updated == issue.updated,
            "decisions changed concurrently; write aborted"
        );
        let result = transport.graphql(
            "mutation DecisionsWrite($id:ID!,$body:String!){updateIssue(input:{id:$id,body:$body}){issue{id body}}}",
            json!({"id":issue.id,"body":next}),
        )?;
        ensure!(
            result
                .pointer("/updateIssue/issue/id")
                .and_then(Value::as_str)
                == Some(issue.id.as_str()),
            "issue update not confirmed; baseline unchanged"
        );
        ensure!(
            result
                .pointer("/updateIssue/issue/body")
                .and_then(Value::as_str)
                == Some(next.as_str()),
            "issue body update not confirmed; baseline unchanged"
        );
    }
    Ok(ResultBody {
        output: json!({"applied":apply,"body":next}),
        written: apply.then_some(next),
        high_water,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::VecDeque};

    const ORIGINAL: &str = "# Decisions\n\n### D12 Turn on scheduler? (P0, blocks sample/work#7)\nSome context.\n- [ ] A) Yes ← recommended: ready\n- [ ] B) Not yet\n\nReply:\n\n## Done\n\n### D2 Earlier? (P2, blocks sample/work#1)\nReply: old answer\nOutcome: kept\n";

    fn target() -> Target {
        Target {
            repository: "sample/decisions".into(),
            issue: 3,
        }
    }

    fn add() -> Add {
        Add {
            settings: Settings {
                config: "agent-board.json".into(),
            },
            question: "Proceed?".into(),
            context: "Background.".into(),
            options: vec!["Yes".into(), "No".into()],
            recommend: "ready".into(),
            item: "sample/work#7".into(),
            priority: "P1".into(),
            apply: false,
        }
    }

    fn response(body: &str, edits: Value) -> Value {
        json!({"viewer":{"login":"sample-bot"},"repository":{"issue":{
            "id":"ISSUE", "body":body, "updatedAt":"2026-01-01T00:00:00Z",
            "userContentEdits":{"nodes":edits,"pageInfo":{"hasNextPage":false}}
        }}})
    }

    // GitHub's userContentEdits shape: newest first, each diff the body that
    // edit left behind.
    fn edit_node(login: &str, at: &str, diff: Value) -> Value {
        json!({"editor":{"login":login},"editedAt":format!("2026-01-0{at}T00:00:00Z"),"diff":diff})
    }

    struct Offline {
        responses: RefCell<VecDeque<Value>>,
        writes: RefCell<Vec<Value>>,
    }

    impl Offline {
        fn new(responses: Vec<Value>) -> Self {
            Self {
                responses: RefCell::new(responses.into()),
                writes: RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for Offline {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            if query.contains("mutation") {
                self.writes.borrow_mut().push(variables.clone());
                Ok(json!({"updateIssue":{"issue":{"id":variables["id"],"body":variables["body"]}}}))
            } else {
                self.responses
                    .borrow_mut()
                    .pop_front()
                    .context("unexpected read")
            }
        }
    }

    #[test]
    fn read_edits_table() {
        for (from, to, ticked, reply) in [
            ("- [ ] B)", "- [x] B)", 1, false),
            ("Reply:\n\n", "Reply:\nPlease wait, exactly.\n\n", 0, true),
            (
                "Turn on scheduler?",
                "Can we defer the scheduler?",
                0,
                false,
            ),
        ] {
            let body = ORIGINAL.replacen(from, to, 1);
            let transport = Offline::new(vec![response(&body, json!([]))]);
            let result = execute(
                &transport,
                &target(),
                Some(ORIGINAL),
                12,
                Operation::Read,
                false,
            )
            .unwrap();
            let changes = result.output["changes"].as_array().unwrap();
            assert_eq!(changes.len(), 1, "{from}");
            assert_eq!(changes[0]["id"], "D12");
            assert_eq!(changes[0]["ticked"].as_array().unwrap().len(), ticked);
            assert_eq!(
                changes[0]["reply"]
                    .as_str()
                    .unwrap()
                    .contains("Please wait, exactly."),
                reply
            );
            assert_eq!(
                changes[0]["after"],
                &body[body.find("### D12").unwrap()..body.find("## Done").unwrap()]
            );
            assert!(result.written.is_none());
            assert!(transport.writes.borrow().is_empty());
        }
    }

    #[test]
    fn writes_merge_and_concurrency_table() {
        let edited = ORIGINAL.replace("Reply:\n\n", "Reply:\nDon't reword this!\n\n");
        for (resolve, apply, concurrent) in [
            (false, false, false),
            (false, true, false),
            (true, true, false),
            (false, true, true),
            (true, true, true),
        ] {
            let second = if concurrent {
                format!("{edited}\nnew operator text\n")
            } else {
                edited.clone()
            };
            let transport = Offline::new(vec![
                response(&edited, json!([])),
                response(&second, json!([])),
            ]);
            let args = add();
            let operation = if resolve {
                Operation::Resolve("D12", "Wait for approval.")
            } else {
                Operation::Add(&args)
            };
            let result = execute(&transport, &target(), Some(ORIGINAL), 12, operation, apply);
            if concurrent {
                assert!(result.err().unwrap().to_string().contains("concurrently"));
                assert!(transport.writes.borrow().is_empty());
            } else {
                let result = result.unwrap();
                let next = result.output["body"].as_str().unwrap();
                let span = &entries(&edited).unwrap()[&12];
                assert!(next.contains(&edited[span.start..span.end]));
                assert!(next.contains("Outcome: kept"));
                if resolve {
                    assert!(entries(next).unwrap()[&12].done);
                    assert!(next.contains("Outcome: Wait for approval."));
                } else {
                    assert!(next.contains("### D13 Proceed? (P1, blocks sample/work#7)"));
                    assert!(next.find("### D13").unwrap() < next.find("## Done").unwrap());
                }
                assert_eq!(transport.writes.borrow().len(), usize::from(apply));
                assert_eq!(result.written.is_some(), apply);
            }
        }
    }

    #[test]
    fn history_fallback_table() {
        let edited = ORIGINAL.replace("Some context.", "Operator's context.");
        let human = edit_node("sample-human", "3", json!(edited));
        let bot = edit_node("sample-bot", "2", json!(ORIGINAL));
        let older = edit_node("sample-bot", "1", json!(""));
        for (name, edits, expected) in [
            ("operator after bot", json!([human, bot]), true),
            ("newest bot write wins", json!([human, bot, older]), true),
            ("oldest-first refused", json!([bot, human]), false),
            (
                "redacted bot diff",
                json!([human, edit_node("sample-bot", "2", Value::Null)]),
                false,
            ),
            ("no bot edit", json!([human]), false),
            ("no history", json!([]), false),
        ] {
            let transport = Offline::new(vec![response(&edited, edits)]);
            let result = execute(&transport, &target(), None, 0, Operation::Read, false);
            assert_eq!(result.is_ok(), expected, "{name}");
            if expected {
                let changes = &result.unwrap().output["changes"];
                assert_eq!(changes.as_array().unwrap().len(), 1, "{name}");
                assert_eq!(changes[0]["id"], "D12", "{name}");
            }
        }
        let transport = Offline::new(vec![response(ORIGINAL, json!([bot]))]);
        assert!(
            execute(&transport, &target(), None, 0, Operation::Read, false)
                .unwrap()
                .output["changes"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn resolution_ignores_fenced_done_heading() {
        for fence in ["```", "~~~"] {
            let body =
                format!("# Decisions\n\n{fence}\n## Done\n{fence}\n\n### D1 Question\nReply:\n");
            let transport =
                Offline::new(vec![response(&body, json!([])), response(&body, json!([]))]);
            let result = execute(
                &transport,
                &target(),
                Some(&body),
                1,
                Operation::Resolve("D1", "Approved."),
                true,
            )
            .unwrap();
            let next = result.written.unwrap();
            assert!(entries(&next).unwrap()[&1].done);
            assert!(next.contains("Outcome: Approved."));
            assert!(edit(&next, Some(&next), 1, &Operation::Resolve("D1", "Again.")).is_err());
        }
    }

    #[test]
    fn history_recovery_refuses_redaction_but_tolerates_old_malformed_structure() {
        for (previous, local, success) in [
            (json!(null), false, false),
            (json!(null), true, true),
            (
                json!("### D40 Previously valid\n### D40 Accidental duplicate\n"),
                false,
                true,
            ),
            (
                json!("### D40 Previously valid\n```\nUnclosed old fence"),
                true,
                true,
            ),
        ] {
            let history = json!([
                edit_node("sample-bot", "2", json!(ORIGINAL)),
                edit_node("sample-human", "1", previous.clone())
            ]);
            let transport = Offline::new(vec![response(ORIGINAL, history)]);
            let args = add();
            let result = execute(
                &transport,
                &target(),
                local.then_some(ORIGINAL),
                12,
                Operation::Add(&args),
                false,
            );
            assert_eq!(
                result.is_ok(),
                success,
                "previous={previous}, local={local}"
            );
            if success && !local {
                assert!(
                    result.unwrap().output["body"]
                        .as_str()
                        .unwrap()
                        .contains("### D41 ")
                );
            }
        }
    }

    #[test]
    fn state_publication_is_private_and_refuses_existing_temporary_file() {
        let dir = std::env::temp_dir().join(format!("decisions-state-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.md");
        save_state(&path, "synthetic private body").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "synthetic private body"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let temp = path.with_extension("md.tmp");
        std::fs::write(&temp, "preexisting").unwrap();
        assert!(save_state(&path, "replacement").is_err());
        assert_eq!(std::fs::read_to_string(&temp).unwrap(), "preexisting");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "synthetic private body"
        );
        std::fs::remove_file(&temp).unwrap();
        save_state(&path, "replacement").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "replacement");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ids_and_validation_table() {
        let args = add();
        let next = edit("", Some(""), 99, &Operation::Add(&args)).unwrap();
        assert!(next.starts_with("### D100 "));
        for id in ["D0", "D01", "D-1", "D", "12"] {
            assert!(decision_id(id).is_err());
        }
        for body in [
            "### D1 q\n### D1 again\n",
            "## Done\n## Done\n",
            "```\nopen",
        ] {
            assert!(entries(body).is_err());
        }
        for (question, context) in [
            ("q\n### D1 bad", "context"),
            ("q", "## Done"),
            ("q", "```text"),
        ] {
            let mut args = add();
            args.question = question.into();
            args.context = context.into();
            assert!(edit("", None, 0, &Operation::Add(&args)).is_err());
        }
        for (id, answer) in [("D99", "ok"), ("D2", "ok"), ("D12", "bad\n## Done")] {
            assert!(
                edit(
                    ORIGINAL,
                    Some(ORIGINAL),
                    12,
                    &Operation::Resolve(id, answer)
                )
                .is_err()
            );
        }
        let transport = Offline::new(vec![response("", json!([])), response("", json!([]))]);
        assert!(execute(&transport, &target(), None, 0, Operation::Add(&args), true).is_ok());
        let mut incomplete = response(ORIGINAL, json!([]));
        incomplete["repository"]["issue"]["userContentEdits"]["pageInfo"]["hasNextPage"] =
            json!(true);
        let transport = Offline::new(vec![incomplete]);
        assert!(execute(&transport, &target(), None, 0, Operation::Add(&args), true).is_err());
    }
}
