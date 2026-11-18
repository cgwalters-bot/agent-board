//! Observe whole, unchanged overlap clusters. These observations authorize no
//! writes: the deployed new-PR handback does not preserve commit series.

use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::Duration;

const MAX_COMPONENT: usize = 8;
const MAX_TRACKED: usize = 64;
const MAX_GAP: Duration = Duration::hours(1);

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Seen {
    since: i64,
    last: i64,
}

pub(crate) type History = BTreeMap<String, Seen>;
pub(super) type Observations = BTreeMap<u64, Observation>;

#[derive(Clone)]
pub(super) enum Observation {
    Unread,
    Excluded(Evidence),
    DependencyHold(Option<Evidence>),
    Candidate(Candidate),
}

impl Observation {
    pub(super) fn excluded(pull: &Value) -> Self {
        Evidence::from_pull(pull).map_or(Self::Unread, Self::Excluded)
    }

    pub(super) fn parked(pull: &Value) -> Self {
        Evidence::from_pull(pull)
            .filter(Evidence::parked)
            .map_or(Self::Unread, Self::Excluded)
    }

    pub(super) fn quiet(listed: &Value) -> Self {
        Evidence::from_listing(listed).map_or(Self::Unread, Self::Excluded)
    }

    pub(super) fn dependency_hold(pull: &Value, unread: bool) -> Self {
        Self::DependencyHold(if unread {
            None
        } else {
            Evidence::from_pull(pull)
        })
    }
}

/// Match the two read surfaces even for exclusions. A stale exclusion can
/// hide a member just as easily as a stale candidate can add one.
#[derive(Clone, PartialEq)]
pub(super) struct Evidence {
    head: String,
    base: String,
    draft: bool,
    held: bool,
    needs_approval: bool,
}

impl Evidence {
    fn read(head: &str, base: &str, draft: &Value, labels: &Value) -> Option<Self> {
        if !hex(head, 40) || !hex(base, 40) {
            return None;
        }
        let labels = labels.as_array()?;
        let has_label = |name| {
            labels
                .iter()
                .any(|l| text(l, "/name").eq_ignore_ascii_case(name))
        };
        Some(Self {
            head: head.into(),
            base: base.into(),
            draft: draft.as_bool()?,
            held: holds(labels) || has_label(LABEL),
            needs_approval: has_label(OPERATOR_APPROVAL_LABEL),
        })
    }

    fn from_pull(pull: &Value) -> Option<Self> {
        if pull
            .pointer("/labels/pageInfo/hasNextPage")
            .and_then(Value::as_bool)
            != Some(false)
        {
            return None;
        }
        Self::read(
            text(pull, "/headRefOid"),
            text(pull, "/baseRefOid"),
            &pull["isDraft"],
            &pull["labels"]["nodes"],
        )
    }

    fn from_listing(listed: &Value) -> Option<Self> {
        Self::read(
            text(listed, "/head/sha"),
            text(listed, "/base/sha"),
            &listed["draft"],
            &listed["labels"],
        )
    }

    fn parked(&self) -> bool {
        self.draft || self.held
    }
}

#[derive(Clone)]
pub(super) struct Candidate {
    number: u64,
    evidence: Evidence,
    branch: String,
    source: (String, String),
    paths: BTreeSet<String>,
    blocker: Option<u64>,
}

fn hex(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(super) fn from_json(value: Option<&Value>) -> Result<History> {
    let Some(value) = value else {
        return Ok(History::new());
    };
    let map = value
        .as_object()
        .context("poll state overlap_recovery is not a map")?;
    ensure!(
        map.len() <= MAX_TRACKED,
        "too many overlap_recovery observations"
    );
    let history: History = serde_json::from_value(value.clone())
        .context("invalid poll state overlap_recovery observation")?;
    ensure!(
        history
            .iter()
            .all(|(key, seen)| hex(key, 64) && seen.since >= 0 && seen.since <= seen.last),
        "invalid poll state overlap_recovery identity or times"
    );
    Ok(history)
}

pub(super) fn observe(
    repo: &str,
    pull: &Value,
    config: &Config,
    review_run: Option<&Value>,
    repair: bool,
    blocker: Option<u64>,
) -> Observation {
    use Observation::*;
    let Some(evidence) = Evidence::from_pull(pull) else {
        return Unread;
    };
    if evidence.parked() || pull["baseRepository"]["isPrivate"] == true || no_merge(config, pull) {
        return Excluded(evidence);
    }
    let default = text(pull, "/baseRepository/defaultBranchRef/name");
    if text(pull, "/author/login") != config.bot
        || pull["baseRepository"]["isPrivate"] != false
        || !text(pull, "/baseRepository/nameWithOwner").eq_ignore_ascii_case(repo)
        || default.is_empty()
        || !truncated(pull).is_empty()
        || pull.pointer("/reviewRequests/pageInfo/hasNextPage") != Some(&Value::Bool(false))
        || pull
            .pointer("/reviewRequests/nodes")
            .and_then(Value::as_array)
            .is_none()
    {
        return Unread;
    }
    if text(pull, "/baseRefName") != default
        || needs_operator_approval(pull)
        || !pull["mergeQueueEntry"].is_null()
        || !pull["autoMergeRequest"].is_null()
        // The ordinary decision's hold policy: a pending operator review
        // parks the head only with `assignHolds`.
        || matches!(review_hold(config, pull), Some(ReviewHold::Pending))
        || review_at_head(pull, config, review_run) != Some(Review::Approved)
        || !reviewed_on_base(pull, config, review_run)
    {
        return Excluded(evidence);
    }
    if !matches!(pull["mergeable"].as_str(), Some(CONFLICTING | "MERGEABLE")) {
        return Unread;
    }
    if !repair {
        return Excluded(evidence);
    }
    let Some(number) = pull["number"].as_u64().filter(|n| *n > 0) else {
        return Unread;
    };
    let source_repo = text(pull, "/headRepository/nameWithOwner");
    let source_ref = text(pull, "/headRefName");
    // The GraphQL files list has no previous name for a rename: its paths
    // cannot establish component membership, even with a complete page.
    if pull.pointer("/files/pageInfo/hasNextPage") != Some(&Value::Bool(false)) {
        return Unread;
    }
    let paths = pull
        .pointer("/files/nodes")
        .and_then(Value::as_array)
        .and_then(|files| {
            files
                .iter()
                .map(|f| {
                    matches!(
                        f["changeType"].as_str(),
                        Some("ADDED" | "MODIFIED" | "DELETED")
                    )
                    .then(|| {
                        f["path"]
                            .as_str()
                            .filter(|p| !p.is_empty())
                            .map(str::to_owned)
                    })
                    .flatten()
                })
                .collect::<Option<BTreeSet<_>>>()
        });
    let Some(paths) = paths.filter(|p| !p.is_empty()) else {
        return Unread;
    };
    if validate_repository(source_repo).is_err() || source_ref.is_empty() {
        return Unread;
    }
    Candidate(self::Candidate {
        number,
        evidence,
        branch: default.into(),
        source: (source_repo.to_ascii_lowercase(), source_ref.into()),
        paths,
        blocker,
    })
}

/// A census from the cap and whole decisions of its possible members must
/// agree. In particular, the approved search alone is not a whole census.
fn candidates<'a>(
    open: &[Value],
    observed: &'a Observations,
    bot: &str,
) -> Option<Vec<&'a Candidate>> {
    let mut pulls = Vec::new();
    let mut matched = 0;
    for listed in open
        .iter()
        .filter(|p| text(p, "/user/login").eq_ignore_ascii_case(bot))
    {
        let evidence = Evidence::from_listing(listed)?;
        let observation = observed.get(&listed["number"].as_u64()?);
        matched += usize::from(observation.is_some());
        match observation {
            None if evidence.parked() => {}
            Some(Observation::Excluded(seen) | Observation::DependencyHold(Some(seen)))
                if *seen == evidence => {}
            Some(Observation::Candidate(c)) if c.evidence == evidence => pulls.push(c),
            _ => return None,
        }
    }
    pulls.sort_unstable_by_key(|c| c.number);
    // The action-first walk can discover a new PR after the cap census.
    (matched == observed.len()).then_some(pulls)
}

fn components(mut rest: Vec<&Candidate>) -> Vec<Vec<&Candidate>> {
    let mut groups = Vec::new();
    while let Some(seed) = rest.pop() {
        let mut group = vec![seed];
        let mut i = 0;
        while i < group.len() {
            let at = group[i];
            rest.retain(|other| {
                if at.paths.is_disjoint(&other.paths) {
                    true
                } else {
                    group.push(other);
                    false
                }
            });
            i += 1;
        }
        // A heuristic cap hold (unread files, scan bound, or an unrelated
        // blocker) is not evidence of an actual overlap within this group.
        let blockers: Vec<_> = group
            .iter()
            .filter_map(|c| c.blocker.map(|b| (*c, b)))
            .collect();
        if (2..=MAX_COMPONENT).contains(&group.len())
            && group
                .iter()
                .all(|c| c.evidence.base == seed.evidence.base && c.branch == seed.branch)
            && !blockers.is_empty()
            && blockers.iter().all(|(c, b)| {
                group
                    .iter()
                    .any(|p| p.number == *b && !p.paths.is_disjoint(&c.paths))
            })
        {
            group.sort_unstable_by_key(|c| c.number);
            groups.push(group);
        }
    }
    groups.sort_unstable_by_key(|c| c[0].number);
    groups
}

/// Only a completely walked repository with a bounded open census can age,
/// and not once this pass has moved the base that census was read against.
pub(super) fn eligible_open<'a>(
    pressure: &'a Pressure,
    undecided: &BTreeSet<String>,
    repo: &str,
) -> Option<&'a [Value]> {
    let open = pressure.open.get(repo)?;
    (open.len() <= crate::pass::pressure::OLDER_LIMIT
        && !undecided.contains(repo)
        && !pressure.landed.contains(repo))
    .then_some(open.as_slice())
}

pub(super) fn advance(
    pressure: &Pressure,
    old: &History,
    (undecided, lines): (&BTreeSet<String>, &mut Vec<String>),
    scope: (&str, &str, &str),
    hours: u16,
    now: OffsetDateTime,
) -> History {
    let now = now.unix_timestamp();
    let mut next = History::new();
    for (repo, observed) in &pressure.recovery {
        let Some(open) = eligible_open(pressure, undecided, repo) else {
            continue;
        };
        let Some(pulls) = candidates(open, observed, scope.2) else {
            continue;
        };
        // A repair this pass started on any of the repository's pull
        // requests, in whichever turn, or through any pull request sharing
        // a member's branch (a paired upstream's, say) outdates the census.
        let started = &pressure.started;
        if started.items.iter().any(|(at, number)| {
            at == repo
                && (observed.contains_key(number)
                    || open.iter().any(|p| p["number"].as_u64() == Some(*number)))
        }) || pulls.iter().any(|c| started.branches.contains(&c.source))
        {
            continue;
        }
        for group in components(pulls) {
            if next.len() == MAX_TRACKED {
                return next;
            }
            let members: Vec<_> = group
                .iter()
                .map(|c| {
                    (
                        c.number,
                        &c.evidence.head,
                        &c.evidence.base,
                        &c.branch,
                        &c.source,
                    )
                })
                .collect();
            // Hosted callers can carry this body-free history without carrying
            // the credential-scoped response cache. It never authorizes writes.
            let key = format!(
                "{:x}",
                Sha256::digest(json!([scope, hours, repo, members]).to_string())
            );
            let since = old
                .get(&key)
                .filter(|seen| seen.last <= now && now - seen.last <= MAX_GAP.whole_seconds())
                .map_or(now, |seen| seen.since);
            next.insert(key, Seen { since, last: now });
            if now - since >= Duration::hours(hours.into()).whole_seconds() {
                let pulls = group
                    .iter()
                    .map(|c| format!("#{}", c.number))
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(format!("{repo}: overlap recovery for {pulls} held: caller must preserve ordered commits when opening one checked replacement PR"));
            }
        }
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCOPE: (&str, &str, &str) = ("o/caller", "operator", "bot");

    fn candidate(number: u64, paths: &[&str], blocker: Option<u64>) -> Candidate {
        Candidate {
            number,
            evidence: Evidence {
                head: format!("{number:040x}"),
                base: "a".repeat(40),
                draft: false,
                held: false,
                needs_approval: false,
            },
            branch: "main".into(),
            source: ("bot/repo".into(), format!("work/{number}")),
            paths: paths.iter().map(|p| (*p).into()).collect(),
            blocker,
        }
    }

    fn pressure(pulls: Vec<Candidate>) -> Pressure {
        let open = pulls.iter().map(|c| json!({"number":c.number,"draft":false,"labels":[],"user":{"login":"bot"},"head":{"sha":c.evidence.head},"base":{"sha":c.evidence.base}})).collect();
        Pressure {
            open: BTreeMap::from([("o/r".into(), open)]),
            recovery: BTreeMap::from([(
                "o/r".into(),
                pulls
                    .into_iter()
                    .map(|c| (c.number, Observation::Candidate(c)))
                    .collect(),
            )]),
            ..Pressure::default()
        }
    }

    #[test]
    fn overlap_recovery_age_boundary_and_reset() {
        let original = || vec![candidate(1, &["a"], None), candidate(2, &["a"], Some(1))];
        let now = OffsetDateTime::parse("2026-10-01T00:00:00Z", &Rfc3339).unwrap();
        let initial = advance(
            &pressure(original()),
            &History::new(),
            (&BTreeSet::new(), &mut Vec::new()),
            SCOPE,
            72,
            now,
        );
        let mut history = initial.clone();
        for h in 1..=71 {
            let mut outcome = Outcome::default();
            history = advance(
                &pressure(original()),
                &history,
                (&outcome.undecided, &mut outcome.lines),
                SCOPE,
                72,
                now + Duration::hours(h),
            );
            assert!(outcome.lines.is_empty());
        }
        for case in [
            "before",
            "at",
            "duplicate",
            "head",
            "base",
            "membership",
            "gap",
            "future",
            "unread",
            "landed",
            "started",
            "disjoint",
            "issue",
            "moved",
        ] {
            let mut pulls = original();
            let mut old = history.clone();
            let mut at = now + Duration::hours(72);
            match case {
                "before" => at -= Duration::seconds(1),
                "head" => pulls[0].evidence.head = "b".repeat(40),
                "base" => pulls
                    .iter_mut()
                    .for_each(|c| c.evidence.base = "c".repeat(40)),
                "membership" => pulls.push(candidate(3, &["a"], Some(1))),
                "gap" => at += Duration::seconds(1),
                "future" => old
                    .values_mut()
                    .for_each(|s| s.last = at.unix_timestamp() + 1),
                _ => (),
            }
            let mut pressure = pressure(pulls);
            // An open PR outside the cluster, its decision excluding it
            // because a repair started on it.
            let disjoint = candidate(5, &["z"], None);
            let listed = json!({"number":5,"draft":false,"labels":[],"user":{"login":"bot"},"head":{"sha":disjoint.evidence.head},"base":{"sha":disjoint.evidence.base}});
            pressure.open.get_mut("o/r").unwrap().push(listed);
            pressure
                .recovery
                .get_mut("o/r")
                .unwrap()
                .insert(5, Observation::Excluded(disjoint.evidence));
            // This pass's own progress, in any turn: a merge into the base,
            // or a repair started on one of the repository's PRs. Only an
            // issue's start, which no census member can share, leaves it.
            match case {
                "unread" => _ = pressure.recovery.get_mut("o/r").unwrap().remove(&2),
                "landed" => _ = pressure.landed.insert("o/r".into()),
                "started" => pressure.started(&Starts {
                    items: BTreeSet::from([("O/r".into(), 2)]),
                    ..Starts::default()
                }),
                "disjoint" | "issue" => pressure.started(&Starts {
                    items: BTreeSet::from([("o/r".into(), if case == "issue" { 9 } else { 5 })]),
                    ..Starts::default()
                }),
                // A fix started through a paired upstream PR, not a member,
                // pushes member #2's branch.
                "moved" => pressure.started(&Starts {
                    items: BTreeSet::from([("upstream/r".into(), 9)]),
                    branches: BTreeSet::from([("bot/repo".into(), "work/2".into())]),
                }),
                _ => (),
            }
            let mut outcome = Outcome::default();
            let next = advance(
                &pressure,
                &old,
                (&outcome.undecided, &mut outcome.lines),
                SCOPE,
                72,
                at,
            );
            assert_eq!(
                outcome.lines.len(),
                usize::from(matches!(case, "at" | "duplicate" | "issue")),
                "{case}"
            );
            if matches!(case, "unread" | "landed" | "started" | "disjoint" | "moved") {
                assert!(next.is_empty(), "{case}");
            } else {
                let seen = next.values().next().unwrap();
                assert_eq!(
                    seen.since,
                    if matches!(case, "at" | "before" | "duplicate" | "issue") {
                        now
                    } else {
                        at
                    }
                    .unix_timestamp(),
                    "{case}"
                );
            }
            if case == "duplicate" {
                let mut same = Outcome::default();
                assert_eq!(
                    advance(
                        &pressure,
                        &old,
                        (&same.undecided, &mut same.lines),
                        SCOPE,
                        72,
                        at
                    ),
                    next
                );
                assert_eq!(same.lines, outcome.lines);
            }
        }
        assert!(from_json(Some(&json!({"forged":{"since":0,"last":0}}))).is_err());
    }

    #[test]
    fn overlap_recovery_component_size_bounds_do_not_split_oversized_groups() {
        let mut pulls = vec![
            candidate(1, &["a"], None),
            candidate(2, &["a", "b"], Some(1)),
            candidate(3, &["b"], Some(2)),
        ];
        pulls.extend((4..=12).map(|n| candidate(n, &["other"], (n != 4).then_some(4))));
        for (len, count) in [(11, 2), (12, 1)] {
            let groups = components(pulls[..len].iter().rev().collect());
            assert_eq!(groups.len(), count);
            assert_eq!(
                groups[0].iter().map(|c| c.number).collect::<Vec<_>>(),
                [1, 2, 3]
            );
        }
    }
}
