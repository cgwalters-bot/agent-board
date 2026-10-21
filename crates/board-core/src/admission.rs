//! A deterministic admission envelope for fanning one pass's work out across
//! repositories (agent-board#182): the same inventory, cap and cursor give
//! the same admitted set, in the same order, and the envelope is written down
//! with the pass so a later pass can be compared with it.
//!
//! After githubnext/gh-cao's control precompute (commit
//! 6c350917c00cd579d508623d3495de39304aa095, `.github/workflows/shared/control.mjs`,
//! `createInventory`), which deduplicates and sorts the inventory, digests it
//! as an inventory version, takes one contiguous batch of a fixed size, caps
//! the candidates, and records version, batch identity, cap and candidates in
//! the run's precompute record. Copied: the sorted and deduplicated inventory,
//! its digest, the contiguous slice, the explicit cap and the recorded
//! envelope. Changed: the slice starts at a cursor the pass carries forward
//! rather than a static batch index from policy, so successive passes cover
//! the whole inventory in turn; there are no cells (gh-cao partitions by
//! repository id modulo a cell count, and no ids are known here), no rollout
//! percentage, and the inventory is repository names, not API objects. The
//! threat this bounds is a pass that, told to follow many repositories, acts in
//! all of them at once: it keeps each pass's work to a known, logged subset
//! that the next pass continues from, and a step outside it is refused.
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// What one pass admits, and where the next one starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    /// `sha256:` over the sorted inventory, one name a line: a version of the
    /// whole set, so two passes' envelopes can be told to be of the same one.
    pub inventory: String,
    /// Names in the inventory.
    pub count: usize,
    /// The most this pass admits.
    pub cap: usize,
    /// The names admitted, in inventory order: at most `cap`, contiguous
    /// from the cursor.
    pub admitted: Vec<String>,
    /// The name the next pass starts from; none once this pass reached the
    /// end of the inventory, so the next starts over.
    pub next: Option<String>,
}

/// The envelope of one pass: the first `cap` names of the sorted, deduplicated
/// `inventory` from `cursor` on (from the start without one, or with one past
/// every name), and the name after them as the next cursor. A cursor naming
/// something no longer in the inventory resumes at the first name after it,
/// so a repository leaving the set skips nothing else. Names are compared
/// byte for byte, as the poll's followed set is: no case folding, and the
/// digest takes names without newlines, as repository names are.
pub fn admit<'a>(
    inventory: impl IntoIterator<Item = &'a str>,
    cap: usize,
    cursor: Option<&str>,
) -> Result<Envelope> {
    ensure!(cap > 0, "admission cap must be at least 1");
    let names: BTreeSet<&str> = inventory.into_iter().collect();
    let mut listed = String::new();
    for name in &names {
        listed.push_str(name);
        listed.push('\n');
    }
    // From the cursor where a name is at or after it; else from the start.
    let from = cursor
        .filter(|cursor| names.range(*cursor..).next().is_some())
        .unwrap_or("");
    let mut rest = names.range(from..);
    let admitted: Vec<String> = rest
        .by_ref()
        .take(cap)
        .map(|name| (*name).to_owned())
        .collect();
    let next = rest.next().map(|name| (*name).to_owned());
    Ok(Envelope {
        inventory: format!("sha256:{:x}", Sha256::digest(listed)),
        count: names.len(),
        cap,
        admitted,
        next,
    })
}

impl Envelope {
    /// Whether `name` is inside this pass's envelope.
    pub fn admits(&self, name: &str) -> bool {
        self.admitted.iter().any(|a| a == name)
    }

    /// The envelope as one line of the pass's record, `key=value` fields, so
    /// a later pass's line can be compared with it.
    pub fn line(&self) -> String {
        format!(
            "envelope: inventory={} count={} cap={} admitted={} next={}",
            self.inventory,
            self.count,
            self.cap,
            self.admitted.join(","),
            self.next.as_deref().unwrap_or("start")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVENTORY: [&str; 5] = ["o/e", "o/a", "o/c", "o/b", "o/d"];

    #[test]
    fn the_same_inputs_give_the_same_envelope_in_stable_order() {
        let first = admit(INVENTORY, 2, None).unwrap();
        // Order and repeats of the input do not change the envelope.
        let shuffled = ["o/d", "o/b", "o/b", "o/a", "o/e", "o/c", "o/a"];
        assert_eq!(admit(shuffled, 2, None).unwrap(), first);
        assert_eq!(first.admitted, ["o/a", "o/b"]);
        assert_eq!(first.next.as_deref(), Some("o/c"));
        assert_eq!((first.count, first.cap), (5, 2));
        assert!(first.inventory.starts_with("sha256:"));
        // A different inventory is a different version; the cap is not part of it.
        assert_ne!(
            admit(INVENTORY[1..].iter().copied(), 2, None)
                .unwrap()
                .inventory,
            first.inventory
        );
        assert_eq!(
            admit(INVENTORY, 3, None).unwrap().inventory,
            first.inventory
        );
        assert_eq!(
            first.line(),
            format!(
                "envelope: inventory={} count=5 cap=2 admitted=o/a,o/b next=o/c",
                first.inventory
            )
        );
    }

    #[test]
    fn the_cap_and_cursor_cover_the_inventory_across_passes() {
        // (cap, cursor) -> (admitted, next)
        for (cap, cursor, admitted, next) in [
            (2, None, vec!["o/a", "o/b"], Some("o/c")),
            (2, Some("o/c"), vec!["o/c", "o/d"], Some("o/e")),
            (2, Some("o/e"), vec!["o/e"], None),
            // A cursor whose name left the inventory resumes after it.
            (2, Some("o/bb"), vec!["o/c", "o/d"], Some("o/e")),
            // Past the end, or at the cap or over it: the whole inventory.
            (2, Some("o/z"), vec!["o/a", "o/b"], Some("o/c")),
            (5, None, vec!["o/a", "o/b", "o/c", "o/d", "o/e"], None),
            (
                50,
                Some("o/a"),
                vec!["o/a", "o/b", "o/c", "o/d", "o/e"],
                None,
            ),
            (1, Some("o/d"), vec!["o/d"], Some("o/e")),
            // Byte order: no case folding, and upper case sorts first.
            (1, Some("O/z"), vec!["o/a"], Some("o/b")),
        ] {
            let envelope = admit(INVENTORY, cap, cursor).unwrap();
            assert_eq!(envelope.admitted, admitted, "cap {cap} cursor {cursor:?}");
            assert_eq!(
                envelope.next.as_deref(),
                next,
                "cap {cap} cursor {cursor:?}"
            );
        }
        // Following the cursor from the start visits every name once, in order.
        let (mut cursor, mut seen) = (None, Vec::new());
        loop {
            let envelope = admit(INVENTORY, 2, cursor.as_deref()).unwrap();
            seen.extend(envelope.admitted);
            cursor = envelope.next;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(seen, ["o/a", "o/b", "o/c", "o/d", "o/e"]);
        assert!(admit(INVENTORY, 0, None).is_err());
        let empty = admit([], 3, Some("o/a")).unwrap();
        assert!(empty.admitted.is_empty() && empty.next.is_none());
        // Case is not folded: two spellings are two names.
        let cased = admit(["o/a", "O/a"], 5, None).unwrap();
        assert_eq!(cased.admitted, ["O/a", "o/a"]);
        assert_eq!(cased.count, 2);
    }

    #[test]
    fn names_outside_the_envelope_are_refused() {
        let envelope = admit(INVENTORY, 2, Some("o/c")).unwrap();
        for name in INVENTORY {
            assert_eq!(
                envelope.admits(name),
                ["o/c", "o/d"].contains(&name),
                "{name}"
            );
        }
        assert!(!envelope.admits("o/cc"));
        assert!(!envelope.admits(""));
    }
}
