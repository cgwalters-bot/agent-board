//! A job plan shown as a Markdown table and its brief (agent-board#779):
//!
//! ```text
//! <!-- agent-board-plan: agent-board/job-plan/v1 sha256:HEX -->
//! | Kind | Item | Runner | Pool | Status |
//! | --- | --- | --- | --- | --- |
//! | implement | `ostreedev/ostree#3665` | hosted | claude | planned |
//!
//! The brief, as Markdown.
//!
//! <!-- /agent-board-plan -->
//! ```
//!
//! The hidden first line names the schema and the SHA-256 of the canonical
//! plan (its JSON object with sorted keys and no whitespace), so the table
//! and brief are read back exactly, and a change to them that does not
//! also change the hash is refused rather than taken for a new plan. The
//! Status cell is not part of the plan: dispatch rewrites it, and puts its
//! record in hidden lines after the first ([`with_record`]), so that one
//! comment carries the plan and what became of it.
use super::{DISPATCH_MARKER, JOB_PLAN_SCHEMA, JobPlan, PLAN_FENCE, code_fence, has_fence};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// The tag of a shown plan's hidden first line.
pub const PLAN_MARKER: &str = "agent-board-plan";
/// The hidden line that ends a shown plan's brief.
pub const PLAN_END: &str = "<!-- /agent-board-plan -->";
/// What the Status cell of a plan not yet dispatched says.
pub const PLANNED: &str = "planned";
/// An optional field left out, in its cell.
const ABSENT: &str = "—";
/// Characters of a Status cell, so a long error keeps the table readable.
const STATUS_LIMIT: usize = 400;

const KIND: &str = "Kind";
const ITEM: &str = "Item";
const RUNNER: &str = "Runner";
const POOL: &str = "Pool";
const EFFORT: &str = "Effort";
const STACKS_ON: &str = "Stacks on";
const STATUS: &str = "Status";
const COLUMNS: &[&str] = &[KIND, ITEM, RUNNER, POOL, EFFORT, STACKS_ON, STATUS];
const REQUIRED: &[&str] = &[KIND, ITEM, RUNNER, POOL, STATUS];

fn marker_prefix() -> String {
    format!("<!-- {PLAN_MARKER}: ")
}

/// The lines of `body` outside fenced code blocks and HTML comment blocks,
/// with their indexes, by the rules [`super::fence_spans`] reads fences by:
/// a plan shown as an example, or hidden in `<!-- -->`, is not one.
fn top_level(body: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut lines = body.lines().enumerate();
    while let Some((i, line)) = lines.next() {
        if let Some((mark, length)) = code_fence(line) {
            for (_, line) in lines.by_ref() {
                if code_fence(line).is_some_and(|(m, l)| m == mark && l >= length)
                    && line.trim().trim_start_matches(mark).is_empty()
                {
                    break;
                }
            }
        } else if line
            .trim_start_matches(' ')
            .strip_prefix("<!--")
            .is_some_and(|rest| line.len() - line.trim_start().len() <= 3 && !rest.contains("-->"))
        {
            for (_, line) in lines.by_ref() {
                if line.contains("-->") {
                    break;
                }
            }
        } else {
            out.push((i, line));
        }
    }
    out
}

/// A one-line HTML comment, as a record's hidden lines are.
fn hidden(line: &str) -> bool {
    let line = line.trim_end();
    line.starts_with("<!-- ") && line.ends_with(" -->")
}

/// A table row's cells, trimmed: `None` for a line that is not one.
fn cells(line: &str) -> Option<Vec<&str>> {
    let inner = line.trim().strip_prefix('|')?.strip_suffix('|')?;
    Some(inner.split('|').map(str::trim).collect())
}

/// A cell's value without the backticks that keep it from linking.
fn value(cell: &str) -> &str {
    cell.strip_prefix('`')
        .and_then(|c| c.strip_suffix('`'))
        .unwrap_or(cell)
}

fn absent(cell: &str) -> bool {
    matches!(value(cell), "" | ABSENT | "-")
}

/// A shown plan's place in its comment.
struct Shown<'a> {
    lines: Vec<&'a str>,
    /// Its marker's line.
    marker: usize,
    /// The hidden record lines after the marker, as a range of lines.
    record: std::ops::Range<usize>,
    /// The table's header and data rows.
    header: usize,
    row: usize,
    schema: &'a str,
    hash: &'a str,
    brief: String,
}

/// The one shown plan in `body`: `None` without its marker at the top level.
fn find(body: &str) -> Result<Option<Shown<'_>>> {
    let prefix = marker_prefix();
    let top = top_level(body);
    let mut markers = top
        .iter()
        .filter(|(_, l)| l.trim_end().starts_with(&prefix) && hidden(l));
    let Some(&(marker, line)) = markers.next() else {
        return Ok(None);
    };
    ensure!(
        markers.next().is_none() && !has_fence(body, PLAN_FENCE),
        "comment holds more than one plan"
    );
    let fields = line.trim_end()[prefix.len()..].trim_end_matches(" -->");
    let (schema, hash) = match fields.split_whitespace().collect::<Vec<_>>()[..] {
        [schema, hash] => (schema, hash.strip_prefix("sha256:").unwrap_or_default()),
        _ => bail!("plan marker is not `{prefix}SCHEMA sha256:HASH -->`"),
    };
    ensure!(
        hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
        "plan marker has no sha256 hash"
    );
    let lines: Vec<&str> = body.lines().collect();
    let mut at = marker + 1;
    while lines.get(at).is_some_and(|l| hidden(l)) {
        at += 1;
    }
    let record = marker + 1..at;
    while lines.get(at).is_some_and(|l| l.trim().is_empty()) {
        at += 1;
    }
    let (header, row) = (at, at + 2);
    let separator = lines.get(at + 1).and_then(|l| cells(l));
    ensure!(
        lines.get(header).and_then(|l| cells(l)).is_some()
            && separator.is_some_and(|s| s.iter().all(|c| {
                let dashes = c.trim_start_matches(':').trim_end_matches(':');
                dashes.len() >= 3 && dashes.bytes().all(|b| b == b'-')
            }))
            && lines.get(row).and_then(|l| cells(l)).is_some(),
        "plan marker is not followed by its table"
    );
    let end = top
        .iter()
        .find(|(i, l)| *i > row && l.trim_end() == PLAN_END)
        .map(|(i, _)| *i)
        .with_context(|| format!("plan is not closed by {PLAN_END}"))?;
    let brief = lines[row + 1..end].join("\n").trim().to_owned();
    Ok(Some(Shown {
        lines,
        marker,
        record,
        header,
        row,
        schema,
        hash,
        brief,
    }))
}

/// The SHA-256 of a plan object's canonical JSON: sorted keys, no
/// whitespace, as `json.dumps(plan, sort_keys=True, separators=(",", ":"),
/// ensure_ascii=False)` writes it.
pub fn canonical_hash(plan: &Value) -> Result<String> {
    let object = plan.as_object().context("plan is not a JSON object")?;
    let sorted: BTreeMap<&String, &Value> = object.iter().collect();
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_string(&sorted)?.as_bytes())
    ))
}

/// The plan object a shown plan holds, checked against its hash: `None`
/// for a comment without one.
pub(crate) fn json(body: &str) -> Result<Option<Value>> {
    let Some(shown) = find(body)? else {
        return Ok(None);
    };
    let head = cells(shown.lines[shown.header]).context("plan table header")?;
    let row = cells(shown.lines[shown.row]).context("plan table row")?;
    ensure!(
        head.len() == row.len(),
        "plan table row has {} cells for {} columns",
        row.len(),
        head.len()
    );
    let mut columns = BTreeMap::new();
    for (name, cell) in head.iter().zip(&row) {
        ensure!(
            COLUMNS.contains(name),
            "unknown plan column {name:?}; the columns are {}",
            COLUMNS.join(", ")
        );
        ensure!(
            columns.insert(*name, *cell).is_none(),
            "plan column {name:?} is repeated"
        );
    }
    for name in REQUIRED {
        ensure!(
            columns.contains_key(name),
            "plan table has no {name} column"
        );
    }
    let mut plan = Map::new();
    plan.insert("schema".into(), json!(shown.schema));
    let (repo, item) = value(columns[ITEM])
        .rsplit_once('#')
        .context("plan Item is not OWNER/REPO#N")?;
    plan.insert("repo".into(), json!(repo));
    plan.insert("item".into(), number(item).context("plan Item number")?);
    plan.insert("kind".into(), json!(value(columns[KIND])));
    plan.insert("runner".into(), json!(value(columns[RUNNER])));
    if !absent(columns[POOL]) {
        plan.insert("pool".into(), json!(value(columns[POOL])));
    }
    if let Some(effort) = columns.get(EFFORT).filter(|c| !absent(c)) {
        plan.insert("effort".into(), json!(value(effort)));
    }
    plan.insert("brief".into(), json!(shown.brief));
    if let Some(lower) = columns.get(STACKS_ON).filter(|c| !absent(c)) {
        let lower = value(lower).strip_prefix('#').unwrap_or(value(lower));
        plan.insert(
            "stacks_on".into(),
            number(lower).context("plan Stacks on is not #N")?,
        );
    }
    let plan = Value::Object(plan);
    ensure!(
        canonical_hash(&plan)? == shown.hash.to_ascii_lowercase(),
        "plan table and brief do not match the hash in its marker; \
         a plan's text changes only with its hash (docs/job-plans.md)"
    );
    Ok(Some(plan))
}

fn number(text: &str) -> Result<Value> {
    ensure!(
        !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()),
        "{text:?} is not a number"
    );
    Ok(json!(text.parse::<u64>()?))
}

/// The hash a shown plan's marker names, which with its comment's id is
/// the plan's identity ([`super::plan_id`]): its Status and record change
/// as it is dispatched, so its edit time cannot be.
pub(crate) fn hash(body: &str) -> Option<String> {
    Some(find(body).ok()??.hash.to_ascii_lowercase())
}

/// Whether `body` holds a shown plan's marker, valid or not.
pub(crate) fn marked(body: &str) -> bool {
    let prefix = marker_prefix();
    top_level(body)
        .iter()
        .any(|(_, l)| l.trim_end().starts_with(&prefix))
}

/// The dispatch record kept in a shown plan's comment: its hidden lines,
/// the first of them the dispatch marker's, else none.
pub(crate) fn record_lines(body: &str) -> Vec<&str> {
    let lines = hidden_lines(body);
    let dispatch = format!("<!-- {DISPATCH_MARKER}: ");
    match lines.first() {
        Some(first) if first.starts_with(&dispatch) => lines,
        _ => vec![],
    }
}

/// The hidden lines after a shown plan's marker, a record's or a released
/// one's ([`super::released_line`]), whatever they are.
pub(crate) fn hidden_lines(body: &str) -> Vec<&str> {
    let Ok(Some(shown)) = find(body) else {
        return vec![];
    };
    shown.lines[shown.record.clone()]
        .iter()
        .map(|l| l.trim_end())
        .collect()
}

/// A shown plan's Status cell.
pub(crate) fn status(body: &str) -> Option<String> {
    let shown = find(body).ok()??;
    let head = cells(shown.lines[shown.header])?;
    let row = cells(shown.lines[shown.row])?;
    let at = head.iter().position(|c| *c == STATUS)?;
    row.get(at).map(|c| (*c).to_owned())
}

/// `body` with its shown plan's record replaced by `record` (hidden lines;
/// empty for none) and its Status cell by `status`. Everything else, the
/// plan included, is kept as it is.
pub(crate) fn with_record(body: &str, record: &str, status: &str) -> Result<String> {
    let shown = find(body)?.context("the comment no longer shows a plan")?;
    let head = cells(shown.lines[shown.header]).context("plan table header")?;
    let mut row = cells(shown.lines[shown.row]).context("plan table row")?;
    let at = (head.iter().position(|c| *c == STATUS)).context("plan table has no Status")?;
    let status = cell(status);
    if let Some(slot) = row.get_mut(at) {
        *slot = &status;
    }
    let row = format!("| {} |", row.join(" | "));
    let mut out: Vec<&str> = Vec::with_capacity(shown.lines.len() + 3);
    out.extend(&shown.lines[..=shown.marker]);
    out.extend(record.lines().filter(|l| !l.trim().is_empty()));
    // A record goes right after the marker; a blank line follows only if
    // one was there.
    out.extend(&shown.lines[shown.record.end..shown.row]);
    out.push(&row);
    out.extend(&shown.lines[shown.row + 1..]);
    let mut text = out.join("\n");
    if body.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

/// `text` as one table cell: no newlines or pipes, and bounded.
fn cell(text: &str) -> String {
    let flat: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "/");
    match flat.chars().count() > STATUS_LIMIT {
        true => flat.chars().take(STATUS_LIMIT - 1).chain(['…']).collect(),
        false => flat,
    }
}

/// The plan object `plan` is, with its brief as a shown plan holds it.
fn object(plan: &JobPlan) -> Value {
    let mut value = json!({
        "schema": JOB_PLAN_SCHEMA,
        "repo": plan.repo,
        "item": plan.item,
        "kind": plan.kind,
        "runner": plan.runner,
        "brief": plan.brief.replace("\r\n", "\n").trim(),
    });
    if !plan.pool.is_empty() {
        value["pool"] = json!(plan.pool);
    }
    if let Some(effort) = plan.effort {
        value["effort"] = json!(effort.as_str());
    }
    if let Some(lower) = plan.stacks_on {
        value["stacks_on"] = json!(lower);
    }
    value
}

/// `plan` shown as a table and its brief, with Status [`PLANNED`]. A brief
/// that would not read back exactly (an unclosed code fence, a line that
/// ends the plan or starts another) is refused.
pub fn render(plan: &JobPlan) -> Result<String> {
    let value = object(plan);
    let mut columns = vec![
        (KIND, plan.kind.clone()),
        (ITEM, format!("`{}#{}`", plan.repo, plan.item)),
        (RUNNER, plan.runner.clone()),
        (
            POOL,
            match plan.pool.as_str() {
                "" => ABSENT.into(),
                pool => pool.into(),
            },
        ),
    ];
    if let Some(effort) = plan.effort {
        columns.push((EFFORT, effort.as_str().into()));
    }
    if let Some(lower) = plan.stacks_on {
        columns.push((STACKS_ON, format!("`#{lower}`")));
    }
    columns.push((STATUS, PLANNED.into()));
    let row = |cells: Vec<&str>| format!("| {} |", cells.join(" | "));
    let text = format!(
        "{}{JOB_PLAN_SCHEMA} sha256:{} -->\n{}\n{}\n{}\n\n{}\n\n{PLAN_END}",
        marker_prefix(),
        canonical_hash(&value)?,
        row(columns.iter().map(|(name, _)| *name).collect()),
        row(columns.iter().map(|_| "---").collect()),
        row(columns.iter().map(|(_, cell)| cell.as_str()).collect()),
        value["brief"].as_str().unwrap_or_default(),
    );
    let back = json(&text).context("the brief cannot be shown as Markdown")?;
    ensure!(
        back.as_ref() == Some(&value),
        "the brief cannot be shown as Markdown: it does not read back"
    );
    Ok(text)
}

/// Whether a brief shown as Markdown on a public issue in `owner`'s
/// repositories would notify no one outside it: no `@login`, and no
/// reference to another owner's issue or pull request, which GitHub would
/// mark on that item's timeline. A plan in JSON is code, which links
/// nothing; a producer falls back to it for a brief that is not quiet.
pub fn quiet(brief: &str, owner: &str) -> bool {
    let chars: Vec<char> = brief.chars().collect();
    let word = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/');
    for (i, c) in chars.iter().enumerate() {
        let before = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1).copied();
        // `@login`, but not a mail address or a `user@host`.
        if *c == '@'
            && before.is_none_or(|b| !b.is_ascii_alphanumeric())
            && next.is_some_and(|n| n.is_ascii_alphanumeric())
        {
            return false;
        }
        // `OWNER/REPO#N`.
        if *c == '#' && next.is_some_and(|n| n.is_ascii_digit()) {
            let start = (0..i).rev().take_while(|j| word(chars[*j])).last();
            let reference: String = start
                .map(|s| chars[s..i].iter().collect())
                .unwrap_or_default();
            if let Some((named, _)) = reference.split_once('/')
                && !named.eq_ignore_ascii_case(owner)
            {
                return false;
            }
        }
    }
    // `https://github.com/OWNER/REPO/issues/N`, `.../pull/N` and
    // `.../discussions/N`.
    brief
        .to_ascii_lowercase()
        .split("github.com/")
        .skip(1)
        .all(|rest| {
            let mut parts = rest.split('/');
            let named = parts.next().unwrap_or_default();
            let _repo = parts.next();
            let kind = parts.next().unwrap_or_default();
            !matches!(kind, "issues" | "pull" | "discussions") || named.eq_ignore_ascii_case(owner)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> JobPlan {
        JobPlan::from_json(&json!({
            "schema": JOB_PLAN_SCHEMA,
            "repo": "ostreedev/ostree",
            "item": 3665,
            "kind": "implement",
            "runner": "hosted",
            "pool": "claude",
            "brief": "Follow up on the review.\n\n```sh\ncargo test\n```\n\n- one\n- two",
        }))
        .unwrap()
    }

    #[test]
    fn rendered_plan_reads_back_exactly() {
        let mut stacked = plan();
        stacked.stacks_on = Some(3664);
        stacked.effort = Some(super::super::Effort::Heavy);
        let mut unpooled = plan();
        unpooled.pool = String::new();
        for plan in [plan(), stacked, unpooled] {
            let text = render(&plan).unwrap();
            assert!(text.contains("| planned |"), "{text}");
            assert!(!text.contains("\"brief\""), "{text}");
            let body = format!("Intro.\n\n{text}\n\nGenerated-by: x\n");
            assert_eq!(JobPlan::from_comment(&body).unwrap(), Some(plan.clone()));
            assert_eq!(status(&body).as_deref(), Some(PLANNED));
            assert!(record_lines(&body).is_empty());
        }
        let text = render(&plan()).unwrap();
        // The marker's hash is the canonical plan's, as Python writes it.
        let python = r#"{"brief":"Follow up on the review.\n\n```sh\ncargo test\n```\n\n- one\n- two","item":3665,"kind":"implement","pool":"claude","repo":"ostreedev/ostree","runner":"hosted","schema":"agent-board/job-plan/v1"}"#;
        let expected = format!("{:x}", Sha256::digest(python.as_bytes()));
        assert_eq!(hash(&text), Some(expected));
    }

    #[test]
    fn shown_plan_table() {
        let text = render(&plan()).unwrap();
        let swap = |from: &str, to: &str| text.replacen(from, to, 1);
        // A body, then the error expected, or the plan's kind.
        for (body, expected) in [
            (text.clone(), Ok(Some("implement"))),
            ("No plan.".to_owned(), Ok(None)),
            (text.replace('\n', "\r\n"), Ok(Some("implement"))),
            // Shown as an example, quoted or hidden, it is not a plan.
            (format!("````markdown\n{text}\n````"), Ok(None)),
            (format!("<!--\n{text}\n-->"), Ok(None)),
            (text.lines().map(|l| format!("> {l}\n")).collect(), Ok(None)),
            // Status is not part of the plan.
            (swap("| planned |", "| dispatched |"), Ok(Some("implement"))),
            // Anything else is, and changes only with its hash.
            (
                swap("| implement |", "| research |"),
                Err("do not match the hash"),
            ),
            (swap("| claude |", "| gpt5 |"), Err("do not match the hash")),
            (swap("Follow up", "Follow"), Err("do not match the hash")),
            (swap("| claude |", "| — |"), Err("do not match the hash")),
            (swap(" sha256:", " sha1:"), Err("no sha256 hash")),
            (swap(PLAN_END, ""), Err("not closed")),
            (
                swap("| Pool |", "| Budget |"),
                Err("unknown plan column \"Budget\""),
            ),
            (
                swap("| --- | --- | --- | --- | --- |\n", ""),
                Err("followed by its table"),
            ),
            (format!("{text}\n{text}"), Err("more than one plan")),
            (
                format!("{text}\n```{PLAN_FENCE}\n{{}}\n```"),
                Err("more than one plan"),
            ),
        ] {
            let found = JobPlan::from_comment(&body);
            match (found, expected) {
                (Ok(found), Ok(kind)) => {
                    assert_eq!(found.map(|p| p.kind).as_deref(), kind, "{body}")
                }
                (Err(error), Err(expected)) => {
                    assert!(format!("{error:#}").contains(expected), "{body}: {error:#}")
                }
                (found, _) => panic!("{body}: {found:?}"),
            }
        }
        // A brief that would not read back is refused, not shown.
        for brief in ["```\nunclosed", "x\n<!-- /agent-board-plan -->\ny"] {
            let plan = JobPlan {
                brief: brief.into(),
                ..plan()
            };
            assert!(render(&plan).is_err(), "{brief}");
        }
    }

    #[test]
    fn record_is_kept_in_the_plan_comment() {
        let body = format!("Intro.\n\n{}\n\nFooter.\n", render(&plan()).unwrap());
        let record = "<!-- agent-board-dispatch: 7@sha256:x -->\n<!-- agent-board-batch: o/r#1 -->";
        let recorded =
            with_record(&body, record, "dispatched: [run](https://x/actions/runs/9)").unwrap();
        assert_eq!(record_lines(&recorded), record.lines().collect::<Vec<_>>());
        assert_eq!(
            status(&recorded).as_deref(),
            Some("dispatched: [run](https://x/actions/runs/9)")
        );
        assert_eq!(
            JobPlan::from_comment(&recorded).unwrap(),
            JobPlan::from_comment(&body).unwrap()
        );
        assert!(recorded.starts_with("Intro.\n\n") && recorded.ends_with("\n\nFooter.\n"));
        // A cell stays one cell.
        let noisy = with_record(&recorded, "", "a | b\nc").unwrap();
        assert_eq!(status(&noisy).as_deref(), Some("a / b c"));
        assert!(record_lines(&noisy).is_empty());
        assert_eq!(with_record(&noisy, "", PLANNED).unwrap(), body);
    }

    #[test]
    fn quiet_briefs() {
        for (brief, quiet_in_forge) in [
            ("Fix cgwalters-forge/tracker#12 and #13.", true),
            (
                "See https://github.com/cgwalters-forge/tracker/issues/12.",
                true,
            ),
            ("Mail a@b.example.", true),
            ("Ask @cgwalters.", false),
            ("Rebase ostreedev/ostree#3665.", false),
            ("See https://github.com/ostreedev/ostree/pull/3665.", false),
            (
                "Read https://github.com/ostreedev/ostree/blob/main/README.md.",
                true,
            ),
            ("See https://GitHub.com/ostreedev/ostree/issues/1.", false),
            (
                "See https://github.com/ostreedev/ostree/discussions/2.",
                false,
            ),
        ] {
            assert_eq!(quiet(brief, "cgwalters-forge"), quiet_in_forge, "{brief}");
        }
    }
}
