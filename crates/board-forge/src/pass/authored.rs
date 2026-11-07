//! `authored` in the configuration (agent-board#737): stage 2 also follows
//! the repositories where the bot has an open pull request, read once a pass
//! from the search `author:BOT is:pr is:open`, so its pull requests upstream
//! and in unlisted forks get the same follow-up as those in `follow`. A
//! repository only the search added is never merged in, as if it were in
//! `pr.noMerge`, only the bot's own pull requests are followed there, and
//! outside our own space nothing is written there: no one chose to follow it.
//!
//! The setting is how many repositories one pass adds at most, as each
//! followed pull request spends the poll's one request budget, which those
//! in `follow` need too. They take turns ([`Rotation`]). The hosted pass
//! keeps no state, so by default the turn is the pass's clock: the sorted
//! list is cut into windows of the setting, and each [`SLOT_MINUTES`] the
//! next window's turn comes, so any `ceil(N / setting)` consecutive slots
//! sweep every repository with nothing kept: as many passes where the pass
//! interval is the slot. A pass that comes late skips a window for a round;
//! an interval of a multiple of the slot could skip some for good, so the
//! slot is the hosted pass's interval. With `state`, a
//! [`board_core::admission`] envelope whose cursor the pass keeps there
//! takes its place. One out of its turn is undecided, so what the last pass
//! held there is kept, not dropped.
//!
//! It is stage 2's only round: the pass takes no `repositoriesPerPass`, whose
//! envelope, given the followed set with these in it, could leave the same
//! repository out in step with this rotation every pass.
//!
//! A plain rotation can leave the pull request the operator cares most about
//! unseen for many passes, with a dozen repositories ahead of it. So the
//! repositories that matter go first every pass, outside the rotation
//! ([`turn`]): those the live epics name (in their bodies, as `OWNER/REPO#N`
//! or a link, or as the repository of a sub-issue), those of the threads
//! whose operator direction the intake keeps waiting (only with `state`,
//! where that list is kept), and the upstream of any midstream fork among
//! them. Stage 2 reads the live epics for this itself, in the same pass,
//! and stage 3 reuses that listing rather than read it again, so stage 3
//! sees the epics as they were before stage 2's merges: a sub-issue those
//! close counts from the next pass.
use super::*;
use crate::supervise::encode;

/// The most pages of the search one pass reads, 100 pull requests a page.
/// More fails the sweep rather than follow a different part of it each pass.
pub const AUTHORED_PAGES: usize = 3;

/// The most repositories the setting may add a pass.
pub const AUTHORED_LIMIT: usize = 10;

/// The most repositories a pass adds first, ahead of the rotation and in
/// addition to its turn ([`turn`]); any more take their turn with the rest.
pub const PRIORITY_LIMIT: usize = AUTHORED_LIMIT;

/// The length of one turn of the clock rotation ([`Rotation::Clock`]): the
/// pass interval of tracker's poll.yml, whose `pass next` starts each pass
/// ten minutes after the last one started, so consecutive passes take
/// consecutive windows.
pub const SLOT_MINUTES: i64 = 10;

/// How the repositories the search added take turns.
#[derive(Clone, Copy, Debug)]
pub(super) enum Rotation<'a> {
    /// With `state`: from the cursor the last applying pass kept.
    Cursor(Option<&'a str>),
    /// Without it: by the pass's start time, nothing kept.
    Clock(OffsetDateTime),
}

/// One pass's share of the repositories the search added.
#[derive(Debug, PartialEq)]
pub(super) struct Turn {
    /// Those added first, as a live epic or the intake names them.
    pub prioritized: Vec<String>,
    /// Those whose turn it is in the rotation of the rest.
    pub admitted: Vec<String>,
    /// Where the next pass's rotation starts.
    pub next: Option<String>,
}

/// The repositories, lower case, that a pass follows first: those this
/// pass's live epics `named` ([`named`]), those of the intake's `waiting` threads
/// (`OWNER/REPO#N`), and the upstream of each midstream fork among them by
/// `forks`, upstream to fork, where the notification intake read them. Not
/// the intake's staging routes: those map a fork with issues disabled's
/// upstream to the dispatch repository, which every epic may name.
pub(super) fn priority<'a>(
    named: &[String],
    waiting: impl IntoIterator<Item = &'a str>,
    forks: Option<&BTreeMap<String, String>>,
) -> BTreeSet<String> {
    let mut priority: BTreeSet<String> = named.iter().map(|r| r.to_ascii_lowercase()).collect();
    let threads = waiting.into_iter().filter_map(|t| t.split_once('#'));
    priority.extend(threads.map(|(repo, _)| repo.to_ascii_lowercase()));
    for (upstream, fork) in forks.into_iter().flatten() {
        if priority.contains(&fork.to_ascii_lowercase()) {
            priority.insert(upstream.to_ascii_lowercase());
        }
    }
    priority
}

/// The repositories, lower case, that the `live` epics name: in an epic's
/// body ([`referenced`]) or as the repository of an open sub-issue, as many
/// as the poll may follow at all.
pub(super) fn named(live: &[super::epics::EpicFacts]) -> Vec<String> {
    let mut named = BTreeSet::new();
    for epic in live {
        named.extend(referenced(&epic.body));
        let subs = epic.census.sub_issues.iter();
        named.extend(subs.map(|sub| sub.repo.to_ascii_lowercase()));
    }
    named.into_iter().take(POLL_REPOSITORY_LIMIT).collect()
}

/// This pass's turn of `new`, the repositories the search added that are
/// not followed: first those `priority` names (lower case), up to
/// [`PRIORITY_LIMIT`] within `room`, the repositories the poll may still
/// follow; then up to `per_pass` of the rest, within what room is left, by
/// `rotation`. The priority repositories take none of the rotation's
/// `per_pass`, so it moves on at its usual pace however many there are, and
/// one that stops being named simply rejoins it. With no room left for the
/// rotation, its cursor stays.
pub(super) fn turn(
    new: &[String],
    priority: &BTreeSet<String>,
    (per_pass, room): (usize, usize),
    rotation: Rotation<'_>,
) -> Result<Turn> {
    let prioritized: Vec<String> = (new.iter())
        .filter(|repo| priority.contains(&repo.to_ascii_lowercase()))
        .take(PRIORITY_LIMIT.min(room))
        .cloned()
        .collect();
    let rest = (new.iter())
        .filter(|repo| !prioritized.contains(repo))
        .map(String::as_str);
    let room = per_pass.min(room - prioritized.len());
    let cursor = match rotation {
        Rotation::Cursor(cursor) => cursor,
        Rotation::Clock(now) => {
            let admitted = clock_window(rest, room, now);
            return Ok(Turn {
                prioritized,
                admitted,
                next: None,
            });
        }
    };
    if room == 0 {
        let next = cursor.map(str::to_owned);
        return Ok(Turn {
            prioritized,
            admitted: Vec::new(),
            next,
        });
    }
    let envelope = admission::admit(rest, room, cursor)?;
    Ok(Turn {
        prioritized,
        admitted: envelope.admitted,
        next: envelope.next,
    })
}

/// The `room` of the sorted, deduplicated `inventory` whose turn it is at
/// `now`: the window starting `slot * room` names in, wrapping around, where
/// `slot` counts the [`SLOT_MINUTES`] since the epoch. Any `ceil(N / room)`
/// consecutive slots' windows cover all N names, without a cursor.
fn clock_window<'a>(
    inventory: impl IntoIterator<Item = &'a str>,
    room: usize,
    now: OffsetDateTime,
) -> Vec<String> {
    let names: Vec<&str> = (inventory.into_iter())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let take = room.min(names.len());
    if take == 0 {
        return Vec::new();
    }
    // Reduced mod N first: the product stays within N * room.
    let slot = (now.unix_timestamp().div_euclid(SLOT_MINUTES * 60)).rem_euclid(names.len() as i64);
    let start = (slot as usize * take) % names.len();
    (names.iter().cycle().skip(start).take(take))
        .map(|name| (*name).to_owned())
        .collect()
}

/// The repositories, lower case, that `text` names as the place of an issue
/// or pull request, `OWNER/REPO#N`, or in a link `github.com/OWNER/REPO…`.
/// Nothing else is taken for a name: prose such as `ostree PR 3690` names
/// no repository.
pub(super) fn referenced(text: &str) -> BTreeSet<String> {
    const HOST: &str = "github.com/";
    let name = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    let mut found = BTreeSet::new();
    let mut add = |owner: &str, repo: &str| {
        let repo = repo.trim_end_matches('.');
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        let at = format!("{owner}/{repo}");
        if validate_repository(&at).is_ok() {
            found.insert(at.to_ascii_lowercase());
        }
    };
    for (at, _) in text.match_indices(HOST) {
        // Not `api.github.com/` or another host ending so.
        if text[..at].ends_with(|c: char| name(c)) {
            continue;
        }
        let mut parts = text[at + HOST.len()..].splitn(3, '/');
        if let (Some(owner), Some(repo)) = (parts.next(), parts.next()) {
            let repo = repo.split(|c: char| !name(c)).next().unwrap_or_default();
            add(owner, repo);
        }
    }
    for (at, _) in text.match_indices('#') {
        if !text[at + 1..].starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let before = &text[..at];
        let start = (before.char_indices().rev())
            .find(|&(_, c)| !(name(c) || c == '/' || c.is_alphanumeric()))
            .map_or(0, |(i, c)| i + c.len_utf8());
        if let Some((owner, repo)) = before[start..].split_once('/') {
            add(owner, repo);
        }
    }
    found
}

/// The repositories of `bot`'s open pull requests, sorted by lower-case name.
pub(super) fn read(bot: &str, client: &impl Rest) -> Result<Vec<String>> {
    // An archived repository's pull requests can never move.
    let query = encode(&format!("author:{bot} is:pr is:open archived:false"));
    let mut next = Some(format!("{API_ROOT}search/issues?q={query}&per_page=100"));
    let mut found = BTreeMap::new();
    for _ in 0..AUTHORED_PAGES {
        let Some(url) = next.take() else { break };
        let response = client.get(&url, None)?;
        ensure!(
            response.status == 200,
            "GET {url}: GitHub HTTP status {}",
            response.status
        );
        ensure!(
            response.body["incomplete_results"] != true,
            "the search for {bot}'s pull requests is incomplete"
        );
        let items = (response.body["items"].as_array()).context("search items are not a list")?;
        for item in items {
            let at = item["repository_url"]
                .as_str()
                .context("a pull request has no repository_url")?;
            let name = (at.strip_prefix(&format!("{API_ROOT}repos/")))
                .with_context(|| format!("repository {at} is outside {API_ROOT}"))?;
            validate_repository(name)?;
            ensure!(
                item["pull_request"].is_object() && text(item, "/user/login") == bot,
                "{name}#{} is not a pull request of {bot}'s",
                item["number"]
            );
            found.insert(name.to_ascii_lowercase(), name.to_owned());
        }
        next = response.link.as_deref().and_then(next_link);
        // The token goes wherever the next page is.
        if let Some(url) = next.as_ref().filter(|u| !u.starts_with(API_ROOT)) {
            bail!("the next page {url} is outside {API_ROOT}");
        }
    }
    ensure!(
        next.is_none(),
        "{bot} has more than {AUTHORED_PAGES} pages of open pull requests"
    );
    Ok(found.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With 13 repositories not followed and 3 a pass, as in production,
    /// the one a live epic names is followed every pass, from every place
    /// the cursor can be, and takes none of the rotation's 3, which still
    /// goes round the other 12 in four applying passes.
    #[test]
    fn priority_goes_first_from_every_cursor() {
        let new: Vec<String> = (b'a'..=b'm').map(|c| format!("o/{}", c as char)).collect();
        let priority = BTreeSet::from(["o/g".to_owned()]);
        let cursors = std::iter::once(None).chain(new.iter().map(|r| Some(r.as_str())));
        for cursor in cursors {
            let rotation = Rotation::Cursor(cursor);
            let turn = turn(&new, &priority, (3, POLL_REPOSITORY_LIMIT), rotation).unwrap();
            assert_eq!(turn.prioritized, ["o/g"], "{cursor:?}");
            // Fewer at the end of the round, as the envelope stops there.
            assert!(
                (1..=3).contains(&turn.admitted.len()),
                "{cursor:?}: {turn:?}"
            );
            assert!(!turn.admitted.contains(&"o/g".to_owned()), "{cursor:?}");
        }
        let (mut cursor, mut seen) = (None, BTreeSet::new());
        for _ in 0..4 {
            let rotation = Rotation::Cursor(cursor.as_deref());
            let turn = turn(&new, &priority, (3, POLL_REPOSITORY_LIMIT), rotation).unwrap();
            seen.extend(turn.admitted);
            cursor = turn.next;
        }
        assert_eq!(seen.len(), 12);
        assert_eq!(cursor, None);
    }

    /// Without `state`, consecutive passes a slot apart, each starting from
    /// nothing kept, sweep every repository not prioritized within
    /// `ceil(N / turn)` passes from wherever the clock is, while the
    /// prioritized one comes first every pass and takes none of the turn.
    /// tracker's case is 13 not followed, 3 a pass, one named by an epic.
    #[test]
    fn clock_sweeps_everything_within_a_round_without_state() {
        let start = OffsetDateTime::from_unix_timestamp(1_793_000_000).unwrap();
        let slot = time::Duration::minutes(SLOT_MINUTES);
        for (count, per_pass, room) in [
            (13usize, 3usize, 50usize),
            (12, 3, 50),
            (2, 1, 50),
            (7, 10, 50),
            (13, 3, 3),
        ] {
            let new: Vec<String> = (0..count).map(|n| format!("o/r{n:02}")).collect();
            let named = new[count / 2].clone();
            let priority = BTreeSet::from([named.clone()]);
            let rest = count - 1;
            let take = per_pass.min(room - 1);
            let passes = rest.div_ceil(take);
            // From every place in a round, and mid-slot.
            for offset in 0..(2 * passes as i32) {
                let first = start + slot * offset + time::Duration::seconds(37);
                let mut seen = BTreeSet::new();
                for pass in 0..passes as i32 {
                    let now = first + slot * pass;
                    let turn =
                        turn(&new, &priority, (per_pass, room), Rotation::Clock(now)).unwrap();
                    let case = format!("{count}/{per_pass}/{room} at {offset}+{pass}");
                    assert_eq!(turn.prioritized, std::slice::from_ref(&named), "{case}");
                    assert_eq!(turn.admitted.len(), take.min(rest), "{case}: {turn:?}");
                    assert!(!turn.admitted.contains(&named), "{case}");
                    assert_eq!(turn.next, None, "{case}");
                    // The same slot, the same turn: a rerun repeats it.
                    let again = turn_at(
                        &new,
                        &priority,
                        (per_pass, room),
                        now + time::Duration::seconds(1),
                    );
                    assert_eq!(again, turn.admitted, "{case}");
                    seen.extend(turn.admitted);
                }
                assert_eq!(
                    seen.len(),
                    rest,
                    "{count}/{per_pass}/{room} at {offset}: {seen:?}"
                );
            }
        }
        // Nothing to take, and no room, take nothing.
        let none = turn(&[], &BTreeSet::new(), (3, 50), Rotation::Clock(start)).unwrap();
        assert!(none.admitted.is_empty() && none.prioritized.is_empty());
        let new = vec!["o/a".to_owned(), "o/b".to_owned()];
        let full = turn(&new, &BTreeSet::new(), (3, 0), Rotation::Clock(start)).unwrap();
        assert!(full.admitted.is_empty());
    }

    fn turn_at(
        new: &[String],
        priority: &BTreeSet<String>,
        bounds: (usize, usize),
        now: OffsetDateTime,
    ) -> Vec<String> {
        turn(new, priority, bounds, Rotation::Clock(now))
            .unwrap()
            .admitted
    }

    /// The bounds on a turn: the priority repositories within
    /// [`PRIORITY_LIMIT`] and the poll's room, the rest rotating in what
    /// room is left, and the cursor kept where there is none.
    #[test]
    fn turn_bounds() {
        let new: Vec<String> = (0..15).map(|n| format!("o/r{n:02}")).collect();
        let all: BTreeSet<String> = new.iter().cloned().collect();
        let none = BTreeSet::new();
        let upper = BTreeSet::from(["o/r03".to_owned()]);
        let no: &[&str] = &[];
        // (case, priority, per pass, room, cursor) to (prioritized,
        // admitted, next).
        for (case, priority, per_pass, room, cursor, prioritized, admitted, next) in [
            (
                "plain",
                &none,
                2,
                50,
                None,
                0,
                &["o/r00", "o/r01"][..],
                Some("o/r02"),
            ),
            (
                "over the limit",
                &all,
                2,
                50,
                None,
                PRIORITY_LIMIT,
                &["o/r10", "o/r11"],
                Some("o/r12"),
            ),
            (
                "no room left",
                &all,
                2,
                4,
                Some("o/r09"),
                4,
                no,
                Some("o/r09"),
            ),
            (
                "room for one",
                &upper,
                3,
                2,
                None,
                1,
                &["o/r00"],
                Some("o/r01"),
            ),
            (
                "none at all",
                &none,
                2,
                0,
                Some("o/r05"),
                0,
                no,
                Some("o/r05"),
            ),
        ] {
            let turn = turn(&new, priority, (per_pass, room), Rotation::Cursor(cursor)).unwrap();
            assert_eq!(turn.prioritized.len(), prioritized, "{case}: {turn:?}");
            assert_eq!(turn.admitted, admitted, "{case}");
            assert_eq!(turn.next.as_deref(), next, "{case}");
        }
        // A repository found in another case is still the one named.
        let mixed: Vec<String> = new.iter().map(|r| r.replace("r03", "R03")).collect();
        let turn = turn(&mixed, &upper, (1, 50), Rotation::Cursor(Some("o/r02"))).unwrap();
        assert_eq!(
            (turn.prioritized, turn.admitted),
            (vec!["o/R03".to_owned()], vec!["o/r02".to_owned()])
        );
        assert_eq!(turn.next.as_deref(), Some("o/r04"));
    }

    #[test]
    fn priority_takes_named_waiting_and_their_upstreams() {
        let named = ["cgwalters-forge/Ostree".to_owned(), "o/epic".to_owned()];
        let routes = BTreeMap::from([
            (
                "ostreedev/ostree".to_owned(),
                "cgwalters-forge/ostree".to_owned(),
            ),
            ("other/up".to_owned(), "cgwalters-forge/unnamed".to_owned()),
            ("u/waited".to_owned(), "o/thread".to_owned()),
        ]);
        let waiting = ["O/Thread#3"];
        let found = priority(&named, waiting, Some(&routes));
        let expected = [
            "cgwalters-forge/ostree",
            "o/epic",
            "o/thread",
            "ostreedev/ostree",
            "u/waited",
        ];
        assert_eq!(
            found.iter().map(String::as_str).collect::<Vec<_>>(),
            expected
        );
        // Without the routes, no upstream.
        let found = priority(&named, waiting, None);
        assert!(!found.contains("ostreedev/ostree"));
    }

    #[test]
    fn referenced_names_only_repositories() {
        for (text, expected) in [
            ("Fixes o/r#12.", &["o/r"][..]),
            (
                "(cgwalters-forge/ostree#5), and Org/Repo.js#7",
                &["cgwalters-forge/ostree", "org/repo.js"],
            ),
            (
                "https://github.com/OstreeDev/ostree/pull/3690.",
                &["ostreedev/ostree"],
            ),
            (
                "<https://github.com/o/r.git>, https://github.com/o/s.",
                &["o/r", "o/s"],
            ),
            ("ostree PR 3690, #12, o/r #3, o/r#x", &[]),
            (
                "https://api.github.com/repos/o/r/issues/1 and notgithub.com/o/r",
                &[],
            ),
            ("a/b/c#4 é/x#1 ünïcode/r#1 x/y#1", &["x/y"]),
            ("", &[]),
        ] {
            let found = referenced(text);
            assert_eq!(
                found.iter().map(String::as_str).collect::<Vec<_>>(),
                expected,
                "{text}"
            );
        }
    }
}
