//! `publicState` (docs/pass.md#public-state): `state` and `store` keep no
//! response, hold, watermark or sweep entry of a repository GitHub does not
//! say is public, so that the caller can carry both in a public artifact
//! (agent-board#1048).
//!
//! The concrete threat it stops is a non-public repository's responses,
//! holds, watermarks or sweep coverage reaching that artifact. Visibility is
//! read live, never from the state it guards, and again at the end of the
//! pass rather than trusting stage 0's answer, since a repository made
//! private meanwhile may have been read since. A repository whose visibility
//! is unread counts as not public: it is read again, and walked whole, next
//! pass.
//!
//! A name is not an identity: a private repository can be renamed and its
//! old name given to a public one. So what is kept of a repository is bound
//! to the node id GitHub gave its name when the last pass ended, before
//! anything this pass read, and kept only where the name still has that id
//! now. A repository new to that record, the first pass with the option on
//! included, keeps nothing until the next pass.
use super::*;
use crate::store::{self, Identities};
use std::path::Path;

/// Repositories one visibility query names.
const QUERY_REPOSITORIES: usize = 100;

/// The visibility queries a pass makes, outside its request share.
pub const VISIBILITY_QUERIES: usize = 4;

/// The most repositories a pass asks about, so the most whose identity
/// `state` records.
pub const REPOSITORIES: usize = VISIBILITY_QUERIES * QUERY_REPOSITORIES;

/// The longest repository node id kept; GitHub's are far shorter.
const ID_BYTES: usize = 64;

/// Of [`VISIBILITY_QUERIES`], the most stage 0 makes, so that the end of
/// the pass has the rest.
pub(super) const STAGE_QUERIES: usize = 1;

/// The REST reads under `repos/OWNER/REPO/`, by their whole path, whose
/// answer for a public repository is the same whoever asks: in a pattern,
/// [`NUMBER`] is one number, [`SEGMENT`] one segment and [`REST`], last,
/// one or more. Any other is not kept. That includes reads whose answer
/// depends on the token, such as a pull request's reviews and review
/// comments, where the token's own pending review shows, a collaborator's
/// permission, Actions variables and artifacts, draft releases, a branch
/// and its protection or the repository itself, whose `permissions` are
/// the token's. It also includes reads that show what references an item from
/// elsewhere, such as events, timelines and issue dependencies, where a
/// private repository's commit or issue can appear.
const PUBLIC_READS: [&str; 25] = [
    "actions/jobs/{n}",
    "actions/runs",
    "actions/runs/{n}",
    "actions/runs/{n}/attempts/{n}/jobs",
    "actions/runs/{n}/jobs",
    "actions/workflows/*/runs",
    "check-runs/{n}",
    "check-runs/{n}/annotations",
    "check-suites/{n}",
    "commits",
    "commits/*",
    "commits/*/check-runs",
    "commits/*/pulls",
    "commits/*/status",
    "commits/*/statuses",
    "compare/*",
    "contents/**",
    "git/commits/*",
    "git/ref/heads/*",
    "git/trees/*",
    "issues/{n}",
    "issues/{n}/comments",
    "pulls",
    "pulls/{n}",
    "pulls/{n}/files",
];

/// In [`PUBLIC_READS`], one number.
const NUMBER: &str = "{n}";

/// In [`PUBLIC_READS`], one segment.
const SEGMENT: &str = "*";

/// In [`PUBLIC_READS`], last, one or more segments.
const REST: &str = "**";

pub(super) fn check(config: &Config) -> Result<()> {
    ensure!(
        !config.public_state || config.state.is_some(),
        "publicState needs state"
    );
    Ok(())
}

/// Whether `id` can be a repository node id `state` keeps.
pub(crate) fn valid_id(id: &str) -> bool {
    (1..=ID_BYTES).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-=".contains(&b))
}

/// The pass's visibility queries so far, and the identities the last pass
/// recorded.
#[derive(Debug, Default)]
pub(super) struct Visibility {
    queries: usize,
    recorded: Identities,
}

impl Visibility {
    /// For a pass whose `state` recorded `recorded`.
    pub(super) fn new(recorded: Identities) -> Self {
        Self {
            queries: 0,
            recorded,
        }
    }

    /// Those of `repos`, lower case, that GitHub says are public now, with
    /// their node ids, and how many it does not, asked through `client`
    /// outside the pass's request share in at most `queries` of the pass's
    /// [`VISIBILITY_QUERIES`]. Nothing is remembered between calls. One it
    /// cannot name, or past those queries, is not public.
    pub(super) fn public(
        &mut self,
        client: &impl Transport,
        repos: impl IntoIterator<Item = String>,
        queries: usize,
    ) -> (Identities, usize) {
        let repos: BTreeSet<String> = repos.into_iter().map(|r| r.to_ascii_lowercase()).collect();
        let valid: Vec<&String> = (repos.iter())
            .filter(|repo| validate_repository(repo).is_ok())
            .collect();
        let queries = queries.min(VISIBILITY_QUERIES.saturating_sub(self.queries));
        let asked = valid.len().min(queries * QUERY_REPOSITORIES);
        if asked < valid.len() {
            eprintln!(
                "public state: {} repositories past the {VISIBILITY_QUERIES} queries a pass, so none is kept",
                valid.len() - asked
            );
        }
        let mut public = Identities::new();
        for chunk in valid[..asked].chunks(QUERY_REPOSITORIES) {
            self.queries += 1;
            match read(client, chunk) {
                Ok(read) => public.extend(read),
                Err(error) => eprintln!(
                    "public state: visibility of {} repositories unread, so none is kept: {error:#}",
                    chunk.len()
                ),
            }
        }
        let others = repos.len() - public.len();
        (public, others)
    }

    /// Drop from `state` and `records` what names a repository that is not
    /// public now with the identity the last pass recorded, asking afresh
    /// about every repository they name and those with rows in the `store`
    /// at its path; the store's save drops its older rows of them too, and
    /// those it did not record with that identity ([`store::Records::public`]).
    /// `state` then records the identities of those public now. Cursors
    /// stay: they say where a walk resumes, not what it read. So do intake
    /// refusals, which keep a refused write from being sent again, and
    /// operator-direction receipts.
    pub(super) fn redact(
        &mut self,
        client: &impl Transport,
        store: Option<&Path>,
        state: Option<&mut State>,
        records: &mut store::Records,
    ) -> String {
        let mut names = BTreeSet::new();
        if let Some(state) = &state {
            names.extend(
                (state.responses.keys())
                    .chain(state.pass_responses.keys())
                    .filter_map(|url| read_repository(url)),
            );
            names.extend(
                (state.quiet_waits.keys())
                    .chain(&state.holds_pending)
                    .chain(state.intake_waiting.keys())
                    .chain(state.lock_refused.keys())
                    .map(|item| store::repository(item)),
            );
            names.extend(state.issue_intake.keys().cloned());
            names.extend(state.action_first_positions.keys().cloned());
            if let Some(day) = &state.maintain {
                names.extend(day_repositories(day));
            }
            if let Some(sweep) = &state.changes_sweep {
                names.extend(sweep.covered().iter().cloned());
            }
        }
        names.extend(
            (records.dispatches.values().map(|d| &d.item))
                .chain(records.verdicts.values().map(|v| &v.item))
                .chain(records.plans.keys())
                .chain(
                    records
                        .holds
                        .iter()
                        .flatten()
                        .filter_map(|h| h.item.as_ref()),
                )
                .map(|item| store::repository(item)),
        );
        if let Some(changes) = &records.changes {
            names.extend(changes.repositories.keys().cloned());
            names.extend(changes.pending.iter().map(|item| store::repository(item)));
        }
        // An idle repository's older rows; one unreadable store's are all
        // dropped, as its save rebuilds it.
        match store.map(store::repositories).transpose() {
            Ok(stored) => names.extend(stored.into_iter().flatten()),
            Err(error) => eprintln!("public state: store repositories unread: {error:#}"),
        }
        let (now, others) = self.public(client, names, VISIBILITY_QUERIES);
        let public: Identities = (now.iter())
            .filter(|(repo, id)| self.recorded.get(*repo) == Some(id))
            .map(|(repo, id)| (repo.clone(), id.clone()))
            .collect();
        let mut dropped = 0;
        let mut retain = |kept: bool| {
            dropped += usize::from(!kept);
            kept
        };
        if let Some(state) = state {
            let public = |repo: &String| public.contains_key(repo);
            let url = |url: &str| read_repository(url).is_some_and(|repo| public(&repo));
            let item = |item: &str| public(&store::repository(item));
            let repo = |repo: &str| public(&repo.to_ascii_lowercase());
            state.responses.retain(|key, _| retain(url(key)));
            state.pass_responses.retain(|key, _| retain(url(key)));
            state.quiet_waits.retain(|key, _| retain(item(key)));
            state.holds_pending.retain(|key| retain(item(key)));
            // The `updated_at` each read, watermarks of one item.
            state.intake_waiting.retain(|key, _| retain(item(key)));
            state.lock_refused.retain(|key, _| retain(item(key)));
            state.issue_intake.retain(|key, _| retain(repo(key)));
            (state.action_first_positions).retain(|key, _| retain(repo(key)));
            // Its configuration names the digest's repository and the
            // includes and excludes; losing it only derives the day again.
            if (state.maintain.as_ref())
                .is_some_and(|day| !retain(day_repositories(day).iter().all(public)))
            {
                state.maintain = None;
            }
            if let Some(sweep) = &mut state.changes_sweep {
                sweep.retain_covered(|repo| retain(public(repo)));
            }
            state.public_repositories = now.clone();
        }
        let summary = format!(
            "public state: {} repositories public as last recorded, {} public but new to that record, {others} not; dropped {dropped} state entries of all but the first",
            public.len(),
            now.len() - public.len(),
        );
        records.public = Some(public);
        summary
    }
}

/// The `OWNER/REPO`s of `repos` that GitHub says are public, where its
/// `isPrivate` and `visibility` agree, with their node ids.
fn read(client: &impl Transport, repos: &[&String]) -> Result<Identities> {
    let (query, variables) = query(repos)?;
    let data = client.graphql(&query, variables)?;
    Ok((repos.iter().enumerate())
        .filter_map(|(n, repo)| {
            let repository = &data[format!("r{n}")];
            let id = repository["id"].as_str().filter(|id| valid_id(id))?;
            (repository["isPrivate"] == false && repository["visibility"] == "PUBLIC")
                .then(|| ((*repo).clone(), id.to_owned()))
        })
        .collect())
}

/// The query of `repos`' visibility, and its variables.
fn query(repos: &[&String]) -> Result<(String, Value)> {
    let mut parameters = Vec::new();
    let mut fields = Vec::new();
    let mut variables = serde_json::Map::new();
    for (n, repo) in repos.iter().enumerate() {
        let (owner, name) = repo.split_once('/').context("repository")?;
        parameters.push(format!("$o{n}: String!, $n{n}: String!"));
        fields.push(format!(
            "r{n}: repository(owner: $o{n}, name: $n{n}) {{ id isPrivate visibility }}"
        ));
        variables.insert(format!("o{n}"), owner.into());
        variables.insert(format!("n{n}"), name.into());
    }
    let query = format!(
        "query({}) {{ {} }}",
        parameters.join(", "),
        fields.join(" ")
    );
    Ok((query, Value::Object(variables)))
}

/// The lower-case `OWNER/REPO` a response URL reads where it is one of
/// [`PUBLIC_READS`], `{API_ROOT}repos/OWNER/REPO/READ…`; none for any other.
fn read_repository(url: &str) -> Option<String> {
    let path = url.strip_prefix(API_ROOT)?.strip_prefix("repos/")?;
    let path = path.split(['?', '#']).next()?;
    let mut segments = path.splitn(3, '/');
    let repo = format!("{}/{}", segments.next()?, segments.next()?);
    validate_repository(&repo).ok()?;
    let read: Vec<&str> = segments.next()?.split('/').collect();
    let plain = (read.iter()).all(|s| !s.is_empty() && *s != "." && *s != "..");
    let known = PUBLIC_READS.iter().any(|public| matches(public, &read));
    (known && plain).then(|| repo.to_ascii_lowercase())
}

/// Whether the path `segments` are those of `pattern`, one of
/// [`PUBLIC_READS`].
fn matches(pattern: &str, segments: &[&str]) -> bool {
    let mut segments = segments.iter();
    for expected in pattern.split('/') {
        if expected == REST {
            return segments.next().is_some();
        }
        let Some(segment) = segments.next() else {
            return false;
        };
        let matched = match expected {
            NUMBER => segment.bytes().all(|b| b.is_ascii_digit()),
            SEGMENT => true,
            literal => *segment == literal,
        };
        if !matched {
            return false;
        }
    }
    segments.next().is_none()
}

/// The lower-case repositories `day` names: those it derived, and any its
/// configuration names.
fn day_repositories(day: &maintain::Day) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut values = vec![json!(day)];
    while let Some(value) = values.pop() {
        match value {
            Value::String(s) if validate_repository(&s).is_ok() => {
                found.insert(s.to_ascii_lowercase());
            }
            Value::Array(items) => values.extend(items),
            Value::Object(fields) => values.extend(fields.into_iter().map(|(_, v)| v)),
            _ => {}
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GitHub, where `public` are the public repositories, to their node
    /// ids, and `fail` fails every query.
    #[derive(Default)]
    struct Forge {
        public: RefCell<Identities>,
        fail: bool,
        queries: Cell<usize>,
        asked: Cell<usize>,
    }

    impl Transport for Forge {
        fn graphql(&self, _: &str, variables: Value) -> Result<Value> {
            self.queries.set(self.queries.get() + 1);
            ensure!(!self.fail, "transport failed");
            let mut data = json!({});
            for n in 0.. {
                let Some(owner) = variables[format!("o{n}")].as_str() else {
                    break;
                };
                self.asked.set(self.asked.get() + 1);
                let repo = format!("{owner}/{}", variables[format!("n{n}")].as_str().unwrap());
                let id = self.public.borrow().get(&repo).cloned();
                data[format!("r{n}")] = json!({
                    "id": id.clone().unwrap_or_else(|| "R_private".into()),
                    "isPrivate": id.is_none(),
                    "visibility": if id.is_some() { "PUBLIC" } else { "PRIVATE" }
                });
            }
            Ok(data)
        }
    }

    fn id(repo: &str) -> String {
        format!("R_{}", repo.replace('/', "_"))
    }

    fn identities(repos: &[&str]) -> Identities {
        repos.iter().map(|r| (r.to_string(), id(r))).collect()
    }

    fn forge(public: &[&str]) -> Forge {
        Forge {
            public: RefCell::new(identities(public)),
            ..Forge::default()
        }
    }

    #[test]
    fn public_reads_name_their_repository() {
        for (url, expected) in [
            ("repos/o/r/issues/1", Some("o/r")),
            ("repos/O/R/pulls?per_page=100", Some("o/r")),
            ("repos/o/r/issues/1#agent-board-rest-v1", Some("o/r")),
            (
                "repos/o/r/pulls?state=open#agent-board-rest-v1",
                Some("o/r"),
            ),
            ("repos/o/r/actions/runs/5/jobs", Some("o/r")),
            ("repos/o/r/pulls/2/files?per_page=100", Some("o/r")),
            ("repos/o/r/issues/2/comments?per_page=100", Some("o/r")),
            (
                "repos/o/r/contents/workflow/review.toml?ref=main",
                Some("o/r"),
            ),
            ("repos/o/r/compare/main...abc", Some("o/r")),
            ("repos/o/r/contents/a/b/c", Some("o/r")),
            ("repos/o/r/actions/runs/5/attempts/1/jobs", Some("o/r")),
            ("repos/o/r/branches/main", None),
            // The token's own pending review, and its comments, show here.
            ("repos/o/r/pulls/2/reviews?per_page=100", None),
            ("repos/o/r/pulls/2/reviews/9/comments", None),
            ("repos/o/r/pulls/2/comments?per_page=100", None),
            ("repos/o/r/pulls/x", None),
            ("repos/o/r/branches/main/protection", None),
            ("repos/o/r/releases", None),
            ("repos/o/r/actions/runs/5/artifacts?per_page=2", None),
            (
                "repos/o/r/issues/1/dependencies/blocked_by?per_page=11",
                None,
            ),
            ("repos/o/r/contents", None),
            // The repository itself carries the token's `permissions`.
            ("repos/o/r", None),
            ("repos/o/r#agent-board-rest-v1", None),
            ("repos/o/r/collaborators/someone/permission", None),
            ("repos/o/r/actions/variables/REVIEW_BOT_IDS", None),
            ("repos/o/r/actions/artifacts/1/zip", None),
            ("repos/o/r/issuesx", None),
            ("repos/o/r/rules/branches/main", None),
            // A private repository's references can show here.
            ("repos/o/r/issues/1/events?per_page=100", None),
            ("repos/o/r/issues/1/timeline", None),
            ("repos/o/r/events?per_page=100", None),
            ("repos/o/r/issues/../collaborators", None),
            ("repos/o/r/issues//1", None),
            ("repos/o", None),
            ("repos/o/../x", None),
            ("search/issues?q=repo:o/r", None),
            ("notifications", None),
            ("orgs/o/repos?type=public", None),
            ("user", None),
        ] {
            let url = format!("{API_ROOT}{url}");
            assert_eq!(read_repository(&url).as_deref(), expected, "{url}");
        }
        assert_eq!(
            read_repository("https://example.com/repos/o/r/issues"),
            None
        );
    }

    #[test]
    fn unread_invalid_and_excess_repositories_are_not_public() {
        let public = forge(&["o/public", "o/second"]);
        let mut visibility = Visibility::default();
        let asked = ["o/public", "O/Private", "not a repository", "o/second"];
        assert_eq!(
            visibility.public(&public, asked.map(String::from), VISIBILITY_QUERIES),
            (identities(&["o/public", "o/second"]), 2)
        );
        // One whose id is missing or not one kept is not public either.
        for id in ["", "R/1", &"R".repeat(ID_BYTES + 1)] {
            let odd = forge(&["o/odd"]);
            odd.public.borrow_mut().insert("o/odd".into(), id.into());
            let mut visibility = Visibility::default();
            let (public, _) = visibility.public(&odd, ["o/odd".into()], VISIBILITY_QUERIES);
            assert!(public.is_empty(), "{id:?}");
        }
        assert_eq!(public.queries.get(), 1);
        let failing = Forge {
            fail: true,
            ..forge(&["o/third"])
        };
        let third = ["o/third".into()];
        assert_eq!(
            visibility.public(&failing, third, VISIBILITY_QUERIES),
            (Identities::new(), 1),
            "an unread repository is not public"
        );
        // Stage 0's one query, then the end's the rest, however many
        // repositories either names.
        let many = forge(&[]);
        let repos = |n: usize| (0..n).map(move |n| format!("o/r{n}"));
        let mut visibility = Visibility::default();
        visibility.public(&many, repos(1), STAGE_QUERIES);
        visibility.public(&many, repos(400), VISIBILITY_QUERIES);
        assert_eq!(many.queries.get(), VISIBILITY_QUERIES);
        assert_eq!(many.asked.get(), 1 + 300);
        let (public, others) = visibility.public(&many, ["o/late".into()], VISIBILITY_QUERIES);
        assert_eq!((public.len(), others), (0, 1));
        assert_eq!(many.queries.get(), VISIBILITY_QUERIES);
        let mut visibility = Visibility::default();
        visibility.public(&many, repos(400), STAGE_QUERIES);
        assert_eq!(many.queries.get(), VISIBILITY_QUERIES + STAGE_QUERIES);
    }

    /// A repository keeps its responses only where it is public at the end
    /// of the pass as the repository the last pass recorded under its name:
    /// not where stage 0 read it as public but it was made private since,
    /// nor where another repository was given its name, nor where no pass
    /// recorded it, as on the first pass with the option on. Each pass
    /// records the identities of those public now.
    #[test]
    fn only_a_repository_public_as_last_recorded_is_kept() {
        let other = || [("o/r".to_owned(), "R_other".to_owned())].into();
        for (recorded, now, kept) in [
            (identities(&["o/r"]), identities(&["o/r"]), true),
            (identities(&["o/r"]), Identities::new(), false),
            (identities(&["o/r"]), other(), false),
            (Identities::new(), identities(&["o/r"]), false),
        ] {
            let forge = forge(&["o/r"]);
            let mut visibility = Visibility::new(recorded.clone());
            let (public, _) = visibility.public(&forge, ["o/r".into()], STAGE_QUERIES);
            assert!(public.contains_key("o/r"));
            *forge.public.borrow_mut() = now.clone();
            let mut state = State::default();
            let url = format!("{API_ROOT}repos/o/r/issues/1/comments");
            state.responses.insert(url, json!({"body": []}));
            let mut records = store::Records::default();
            visibility.redact(&forge, None, Some(&mut state), &mut records);
            let case = format!("{recorded:?} {now:?}");
            assert_eq!(!state.responses.is_empty(), kept, "{case}");
            let public = if kept { now.clone() } else { Identities::new() };
            assert_eq!(records.public, Some(public), "{case}");
            assert_eq!(state.public_repositories, now, "{case}");
            assert_eq!(forge.queries.get(), 2);
        }
    }

    /// `state` keeps the identities it recorded, and refuses any that a
    /// visibility query could not have recorded.
    #[test]
    fn recorded_identities_round_trip() {
        let state = State {
            interval_seconds: poll::MIN_INTERVAL_SECONDS,
            public_repositories: identities(&["o/r"]),
            ..State::default()
        };
        assert_eq!(State::from_json(&state.to_json()).unwrap(), state);
        let many: Identities = (0..=REPOSITORIES)
            .map(|n| (format!("o/r{n}"), id("o/r")))
            .collect();
        for invalid in [
            json!([]),
            json!({"o/r": 1}),
            json!({"O/R": "R_1"}),
            json!({"not a repository": "R_1"}),
            json!({"o/r": "R/1"}),
            json!(many),
        ] {
            let mut value = state.to_json();
            value["public_repositories"] = invalid.clone();
            assert!(State::from_json(&value).is_err(), "{invalid}");
        }
    }

    #[test]
    fn redaction_keeps_only_public_repositories() {
        let response = |path: &str| format!("{API_ROOT}{path}");
        let mut state = State::default();
        for path in [
            "repos/o/public/issues/1#agent-board-rest-v1",
            "repos/o/public/collaborators/someone/permission#agent-board-rest-v1",
            "repos/o/public/pulls/2/reviews?per_page=100#agent-board-rest-v1",
            "repos/o/private/issues/1#agent-board-rest-v1",
            "search/issues?q=x#agent-board-rest-v1",
            "repos/o/private/pulls?state=open",
        ] {
            state.responses.insert(response(path), json!({"body": []}));
        }
        for path in ["repos/o/public/pulls", "repos/o/private/pulls", "user"] {
            (state.pass_responses).insert(response(path), json!({"body": []}));
        }
        let wait = json!({"head": "abc", "updated_at": "2026-11-08T00:00:00Z",
            "read_at": "2026-11-08T00:00:00Z", "kind": "promote"});
        state.quiet_waits =
            poll::quiet::from_json(Some(&json!({"o/public#1": wait, "o/private#2": wait})))
                .unwrap();
        state.holds_pending = vec!["o/public#3".into(), "o/private#4".into()];
        state.issue_intake = BTreeMap::from([
            ("o/Public".into(), "2026-11-08T00:00:00Z".into()),
            ("o/private".into(), "2026-11-08T00:00:00Z".into()),
        ]);
        for item in ["o/public#7", "o/private#7"] {
            let at = "2026-11-08T00:00:00Z".to_owned();
            state.intake_waiting.insert(item.into(), at.clone());
            state.lock_refused.insert(item.into(), at);
        }
        state.action_first_positions =
            BTreeMap::from([("o/public".into(), 1), ("o/private".into(), 2)]);
        // The digest's repository is private, though the day's are public.
        state.maintain = Some(maintain::Day {
            from: "2026-11-08T00:00:00Z".into(),
            repos: vec!["o/public".into()],
            done: true,
            configuration: Some(json!({"maintain": {"digest": {"repository": "o/private"}}})),
            comment: None,
        });
        state.scan_cursor = Some("o/private#5".into());
        let mut records = store::Records::default();
        records.plans.insert(
            "o/private#6".into(),
            store::Plan {
                id: "p".into(),
                kind: "implement".into(),
            },
        );
        // An idle public repository's older row is asked about, so kept.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let mut earlier = store::Records::default();
        earlier
            .plans
            .insert("o/idle#1".into(), records.plans["o/private#6"].clone());
        store::save(&path, &earlier, OffsetDateTime::UNIX_EPOCH).unwrap();
        let forge = forge(&["o/public", "o/idle"]);
        let mut visibility = Visibility::new(identities(&["o/public", "o/idle", "o/private"]));
        let summary = visibility.redact(&forge, Some(&path), Some(&mut state), &mut records);
        assert_eq!(
            summary,
            "public state: 2 repositories public as last recorded, 0 public but new to that record, 1 not; dropped 14 state entries of all but the first"
        );
        assert_eq!(forge.queries.get(), 1);
        assert_eq!(
            state.responses.keys().collect::<Vec<_>>(),
            [&response("repos/o/public/issues/1#agent-board-rest-v1")]
        );
        assert_eq!(
            state.pass_responses.keys().collect::<Vec<_>>(),
            [&response("repos/o/public/pulls")]
        );
        assert_eq!(state.quiet_waits.keys().collect::<Vec<_>>(), ["o/public#1"]);
        assert_eq!(state.holds_pending, ["o/public#3"]);
        assert_eq!(
            state.intake_waiting.keys().collect::<Vec<_>>(),
            ["o/public#7"]
        );
        assert_eq!(
            state.lock_refused.keys().collect::<Vec<_>>(),
            ["o/public#7"]
        );
        assert_eq!(state.issue_intake.keys().collect::<Vec<_>>(), ["o/Public"]);
        assert_eq!(
            state.action_first_positions.keys().collect::<Vec<_>>(),
            ["o/public"]
        );
        assert_eq!(state.maintain, None);
        // A cursor says where a walk resumes, not what it read: the one
        // place the serialized state still names the private repository.
        let serialized = state.to_json().to_string();
        assert_eq!(serialized.matches("o/private").count(), 1, "{serialized}");
        assert_eq!(state.scan_cursor.as_deref(), Some("o/private#5"));
        // The store drops the plan when it saves.
        assert_eq!(records.public, Some(identities(&["o/idle", "o/public"])));
    }
}
