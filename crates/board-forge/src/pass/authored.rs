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
//! in `follow` need too. They take turns in a [`board_core::admission`]
//! envelope whose cursor the pass keeps in its state; one out of its turn is
//! undecided, so what the last pass held there is kept, not dropped.
//!
//! It is stage 2's only round: the pass takes no `repositoriesPerPass`, whose
//! envelope, given the followed set with these in it, could leave the same
//! repository out in step with this cursor every pass. Adding that option
//! means moving this cursor only past what its envelope admitted.
//!
//! A plain rotation can leave the pull request the operator cares most about
//! unseen for many passes, with a dozen repositories ahead of it. So the
//! repositories that matter go first every pass, outside the rotation
//! ([`turn`]): those the live epics name (in their bodies, as `OWNER/REPO#N`
//! or a link, or as the repository of a sub-issue), those of the threads
//! whose operator direction the intake keeps waiting, and the upstream of
//! any midstream fork among them. Stage 3 reads the epics after stage 2 has
//! followed, so their names are kept in the state for the next pass rather
//! than read again here.
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

/// The repositories, lower case, that a pass follows first: those the last
/// pass's live epics `named`, those of the intake's `waiting` threads
/// (`OWNER/REPO#N`), and the upstream of each midstream fork among them by
/// the notification intake's `routes`, upstream to fork, where it read them.
pub(super) fn priority<'a>(
    named: &[String],
    waiting: impl IntoIterator<Item = &'a str>,
    routes: Option<&BTreeMap<String, String>>,
) -> BTreeSet<String> {
    let mut priority: BTreeSet<String> = named.iter().map(|r| r.to_ascii_lowercase()).collect();
    let threads = waiting.into_iter().filter_map(|t| t.split_once('#'));
    priority.extend(threads.map(|(repo, _)| repo.to_ascii_lowercase()));
    for (upstream, fork) in routes.into_iter().flatten() {
        if priority.contains(&fork.to_ascii_lowercase()) {
            priority.insert(upstream.to_ascii_lowercase());
        }
    }
    priority
}

/// This pass's turn of `new`, the repositories the search added that are
/// not followed: first those `priority` names (lower case), up to
/// [`PRIORITY_LIMIT`] within `room`, the repositories the poll may still
/// follow; then up to `per_pass` of the rest, within what room is left, in
/// an envelope from `cursor`. The priority repositories take none of the
/// rotation's `per_pass`, so it moves on at its usual pace however many
/// there are, and one that stops being named simply rejoins it. With no
/// room left for the rotation, its cursor stays.
pub(super) fn turn(
    new: &[String],
    priority: &BTreeSet<String>,
    (per_pass, room): (usize, usize),
    cursor: Option<&str>,
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
            let turn = turn(&new, &priority, (3, POLL_REPOSITORY_LIMIT), cursor).unwrap();
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
            let turn = turn(
                &new,
                &priority,
                (3, POLL_REPOSITORY_LIMIT),
                cursor.as_deref(),
            )
            .unwrap();
            seen.extend(turn.admitted);
            cursor = turn.next;
        }
        assert_eq!(seen.len(), 12);
        assert_eq!(cursor, None);
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
            let turn = turn(&new, priority, (per_pass, room), cursor).unwrap();
            assert_eq!(turn.prioritized.len(), prioritized, "{case}: {turn:?}");
            assert_eq!(turn.admitted, admitted, "{case}");
            assert_eq!(turn.next.as_deref(), next, "{case}");
        }
        // A repository found in another case is still the one named.
        let mixed: Vec<String> = new.iter().map(|r| r.replace("r03", "R03")).collect();
        let turn = turn(&mixed, &upper, (1, 50), Some("o/r02")).unwrap();
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
