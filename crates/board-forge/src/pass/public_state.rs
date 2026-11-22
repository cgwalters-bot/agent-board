//! Public pass state carries positions, not the private pass state with
//! fields removed: no repository name, item number or response. A count of
//! applying passes that walked pull requests picks the repository in the fixed `follow`
//! list the PR walk starts at, and how far that repository's open pull
//! requests are rotated, so a stable prefix cannot use up every pass's share.
//! An ordinal in stage 4's candidate order says where a budget-stopped scan
//! resumes, as [`State::scan_cursor`] does by name.
use crate::poll::State;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Position {
    /// Applying passes whose PR walk ran, which only counts up.
    pub turn: u64,
    /// The ordinal of the candidate the last budget-stopped scan visited
    /// last; none scans in order.
    pub scan: Option<usize>,
}

impl Position {
    /// What the next pass starts from, after an applying pass whose PR walk
    /// ran or not and whose stage 4 left `scan`
    /// ([`crate::pass::Report::scan_position`]).
    pub fn next(self, walked: bool, scan: Option<usize>) -> Self {
        Position {
            turn: self.turn.wrapping_add(u64::from(walked)),
            scan,
        }
    }

    /// [`Self::next`] for an applying pass's report, whatever failures it
    /// counts: each walk's position follows that walk alone, as the private
    /// cursors do, so a failure that recurs in another stage cannot pin it.
    pub fn after(self, report: &crate::pass::Report) -> Self {
        self.next(report.followed, report.scan_position)
    }

    /// How far the start repository's open pull requests, in number order,
    /// are rotated: one further each time it starts the walk again.
    pub fn pull_rotation(self, repositories: usize) -> u64 {
        self.turn / u64::try_from(repositories.max(1)).unwrap_or(u64::MAX)
    }
}

/// The state a pass starts from, reconstituted from the trusted configuration.
/// This is only a starting point for the next walk, not authority to read or
/// act on an item.
pub fn state(position: Position, follow: &[String]) -> State {
    let start = u64::try_from(follow.len())
        .ok()
        .filter(|len| *len > 0)
        .and_then(|len| usize::try_from(position.turn % len).ok())
        .and_then(|index| follow.get(index));
    State {
        share_cursor: start.cloned(),
        public: Some(position),
        ..State::default()
    }
}

pub fn decode(bytes: &[u8], follow: &[String]) -> Result<State> {
    let position = serde_json::from_slice(bytes).context("decode public state")?;
    Ok(state(position, follow))
}

/// Replace any legacy file without copying it. A failed write fails the pass.
pub fn save(path: &Path, position: Position) -> Result<()> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("stage public state in {}", dir.display()))?;
    serde_json::to_writer(&mut file, &position)
        .with_context(|| format!("write public state for {}", path.display()))?;
    file.persist(path)
        .with_context(|| format!("replace {} with public state", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn name_reused_during_read_does_not_export_response() {
        let follow = ["org/other".into(), "org/project".into()];
        let mut legacy = State {
            share_cursor: Some("ORG/PROJECT#97".into()),
            scan_cursor: Some("org/private#3".into()),
            holds_pending: vec!["org/private#1".into()],
            ..State::default()
        };
        let url = "https://api.github.com/repos/org/project/issues/1/comments";
        // The name was public before and after the pass, but referred to a
        // private repository while the response was read. Old and new cache
        // entries for the name are equally unsuitable for publication.
        legacy
            .responses
            .insert(url.into(), json!({"body": ["legacy private"]}));
        legacy
            .pass_responses
            .insert(url.into(), json!({"body": ["fresh private"]}));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let legacy = serde_json::to_vec(&legacy.to_json()).unwrap();
        std::fs::write(&path, &legacy).unwrap();
        assert!(decode(&legacy, &follow).is_err());
        save(&path, Position::default().next(true, Some(4))).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, br#"{"turn":1,"scan":4}"#);
        let position = Position {
            turn: 1,
            scan: Some(4),
        };
        assert_eq!(
            decode(&bytes, &follow).unwrap(),
            State {
                share_cursor: Some("org/project".into()),
                public: Some(position),
                ..State::default()
            }
        );
    }

    #[test]
    fn each_repository_and_pull_request_gets_a_turn_even_when_the_walk_always_runs_out() {
        let follow = ["org/b", "org/a", "org/c"].map(str::to_owned);
        let mut position = Position::default();
        let mut starts = Vec::new();
        for _ in 0..7 {
            let state = state(position, &follow);
            starts.push((
                state.share_cursor.unwrap(),
                position.pull_rotation(follow.len()),
            ));
            position = position.next(true, None);
        }
        let expected = [
            ("org/b", 0),
            ("org/a", 0),
            ("org/c", 0),
            ("org/b", 1),
            ("org/a", 1),
            ("org/c", 1),
            ("org/b", 2),
        ]
        .map(|(repo, rotation)| (repo.to_owned(), rotation));
        assert_eq!(starts, expected);
    }

    /// A failure in another stage, such as a Status write that is refused on
    /// every pass, still lets both walks move on; only a pass whose PR walk
    /// never ran keeps its start.
    #[test]
    fn repeated_failures_elsewhere_do_not_pin_either_walk() {
        use crate::pass::Report;
        let follow = ["org/a", "org/b"].map(str::to_owned);
        let failed = |walked: bool, scan| Report {
            failures: 1,
            followed: walked,
            scan_position: scan,
            ..Report::default()
        };
        let start = Position {
            turn: 0,
            scan: Some(3),
        };
        let first = start.after(&failed(true, Some(5)));
        let second = first.after(&failed(true, Some(9)));
        assert_eq!((first.turn, first.scan), (1, Some(5)));
        assert_eq!((second.turn, second.scan), (2, Some(9)));
        let starts = [start, first, second].map(|p| state(p, &follow).share_cursor.unwrap());
        assert_eq!(starts, ["org/a", "org/b", "org/a"]);
        assert_eq!(second.pull_rotation(follow.len()), 1);
        // Stage 4 reports the scan it was given where it never ran.
        let unwalked = second.after(&failed(false, second.scan));
        assert_eq!(unwalked, second);
    }

    #[test]
    fn missing_or_malformed_fields_are_refused() {
        let follow = ["org/project".into()];
        assert_eq!(
            decode(br#"{"turn":4,"scan":null}"#, &follow)
                .unwrap()
                .share_cursor
                .as_deref(),
            Some("org/project")
        );
        assert_eq!(state(Position::default(), &[]).share_cursor, None);
        for bytes in [
            br#"{"scan":null}"#.as_slice(),
            br#"{"turn":-1,"scan":null}"#,
            br#"{"follow_index":1}"#,
            br#"{"turn":1,"scan":"org/project#1"}"#,
        ] {
            assert!(decode(bytes, &follow).is_err(), "{bytes:?}");
        }
    }
}
