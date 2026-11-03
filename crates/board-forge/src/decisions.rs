//! The decisions document's bounded structure, shared by editing and publication.
use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;

pub const BODY_LIMIT: usize = 60_000;

/// Byte spans preserve the operator's text when resolution moves an entry.
#[derive(Clone)]
pub struct Entry {
    pub start: usize,
    pub end: usize,
    pub done: bool,
}

pub struct Document {
    pub entries: BTreeMap<u64, Entry>,
    /// Byte offset of the actual Done heading, or None if it is absent.
    pub done_offset: Option<usize>,
}

pub fn entries(body: &str) -> Result<BTreeMap<u64, Entry>> {
    Ok(parse(body)?.entries)
}

pub fn parse(body: &str) -> Result<Document> {
    ensure!(
        body.len() <= BODY_LIMIT,
        "decisions body exceeds {BODY_LIMIT} bytes"
    );
    let mut entries = BTreeMap::new();
    let mut offset = 0;
    let mut previous = None;
    let mut done = false;
    let mut done_offset = None;
    let mut fence = None;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if let Some((mark, length)) = crate::plan::code_fence(trimmed) {
            match fence {
                Some((m, l))
                    if m == mark
                        && length >= l
                        && trimmed.trim().trim_start_matches(mark).is_empty() =>
                {
                    fence = None
                }
                None => fence = Some((mark, length)),
                _ => {}
            }
        } else if fence.is_none() {
            if trimmed == "## Done" {
                ensure!(!done, "duplicate Done section");
                if let Some(id) = previous.take() {
                    let entry: &mut Entry = entries.get_mut(&id).context("missing entry")?;
                    entry.end = offset;
                }
                done = true;
                done_offset = Some(offset);
            }
            if let Some(rest) = trimmed.strip_prefix("### D") {
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
        }
        offset += line.len();
    }
    ensure!(
        fence.is_none(),
        "unclosed Markdown fence in decisions document"
    );
    Ok(Document {
        entries,
        done_offset,
    })
}

pub fn decision_id(id: &str) -> Result<u64> {
    let number = id.strip_prefix('D').context("expected D<n>")?;
    let n: u64 = number.parse().context("expected D<n>")?;
    ensure!(
        n > 0 && number == n.to_string(),
        "expected canonical positive D<n>"
    );
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_spans_respect_fence_lengths_and_done() {
        let body =
            "### D1 Ship?\n````md\n```\n## Done\n### D99 Example\n````\n## Done\n### D2 Old\n";
        let parsed = entries(body).unwrap();
        assert_eq!(parsed.keys().copied().collect::<Vec<_>>(), [1, 2]);
        assert!(!parsed[&1].done);
        assert!(parsed[&2].done);
        assert!(body[parsed[&1].start..parsed[&1].end].ends_with("````\n"));
    }
}
