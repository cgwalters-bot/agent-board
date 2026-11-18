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
    Excluded,
    Candidate(Candidate),
}

#[derive(Clone)]
pub(super) struct Candidate {
    number: u64,
    head: String,
    base: String,
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
    if pull["isDraft"] == true
        || pull["baseRepository"]["isPrivate"] == true
        || is_held(pull)
        || labelled(pull)
        || no_merge(config, pull)
    {
        return Excluded;
    }
    let default = text(pull, "/baseRepository/defaultBranchRef/name");
    let head = text(pull, "/headRefOid");
    let base = text(pull, "/baseRefOid");
    if pull["isDraft"] != false
        || text(pull, "/author/login") != config.bot
        || pull["baseRepository"]["isPrivate"] != false
        || !text(pull, "/baseRepository/nameWithOwner").eq_ignore_ascii_case(repo)
        || default.is_empty()
        || !hex(head, 40)
        || !hex(base, 40)
        || !truncated(pull).is_empty()
        || pull.pointer("/reviewRequests/pageInfo/hasNextPage") != Some(&Value::Bool(false))
        || pull
            .pointer("/reviewRequests/nodes")
            .and_then(Value::as_array)
            .is_none()
    {
        return Unread;
    }
    if !repair
        || text(pull, "/baseRefName") != default
        || needs_operator_approval(pull)
        || !pull["mergeQueueEntry"].is_null()
        || !pull["autoMergeRequest"].is_null()
        || nodes(pull, "/reviewRequests/nodes")
            .iter()
            .any(|r| text(r, "/requestedReviewer/login").eq_ignore_ascii_case(&config.operator))
        || review_at_head(pull, config, review_run) != Some(Review::Approved)
        || !reviewed_on_base(pull, config, review_run)
    {
        return Excluded;
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
        head: head.into(),
        base: base.into(),
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
    for listed in open
        .iter()
        .filter(|p| text(p, "/user/login").eq_ignore_ascii_case(bot))
    {
        if listed["draft"].as_bool()?
            || holds(nodes(listed, "/labels"))
            || nodes(listed, "/labels").iter().any(|l| {
                [LABEL, crate::stop::SUPERVISOR_LABEL]
                    .iter()
                    .any(|label| text(l, "/name").eq_ignore_ascii_case(label))
            })
        {
            continue;
        }
        match observed.get(&listed["number"].as_u64()?)? {
            Observation::Unread => return None,
            Observation::Excluded => {}
            Observation::Candidate(c) => {
                if c.head != text(listed, "/head/sha") || c.base != text(listed, "/base/sha") {
                    return None;
                }
                pulls.push(c);
            }
        }
    }
    pulls.sort_unstable_by_key(|c| c.number);
    Some(pulls)
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
                .all(|c| c.base == seed.base && c.branch == seed.branch)
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

pub(super) fn advance(
    pressure: &Pressure,
    old: &History,
    outcome: &mut Outcome,
    scope: (&str, &str, &str),
    hours: u16,
    now: OffsetDateTime,
) -> History {
    let now = now.unix_timestamp();
    let mut next = History::new();
    for (repo, observed) in &pressure.recovery {
        let Some(open) = pressure.open.get(repo) else {
            continue;
        };
        if open.len() > crate::pass::pressure::OLDER_LIMIT || outcome.undecided.contains(repo) {
            continue;
        }
        let Some(pulls) = candidates(open, observed, scope.2) else {
            continue;
        };
        for group in components(pulls) {
            if next.len() == MAX_TRACKED {
                return next;
            }
            let members: Vec<_> = group
                .iter()
                .map(|c| (c.number, &c.head, &c.base, &c.branch, &c.source))
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
                outcome.lines.push(format!("{repo}: overlap recovery for {pulls} held: caller must preserve ordered commits when opening one checked replacement PR"));
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
            head: format!("{number:040x}"),
            base: "a".repeat(40),
            branch: "main".into(),
            source: ("bot/repo".into(), format!("work/{number}")),
            paths: paths.iter().map(|p| (*p).into()).collect(),
            blocker,
        }
    }

    fn pressure(pulls: Vec<Candidate>) -> Pressure {
        let open = pulls.iter().map(|c| json!({"number":c.number,"draft":false,"user":{"login":"bot"},"head":{"sha":c.head},"base":{"sha":c.base}})).collect();
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
            &mut Outcome::default(),
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
                &mut outcome,
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
        ] {
            let mut pulls = original();
            let mut old = history.clone();
            let mut at = now + Duration::hours(72);
            match case {
                "before" => at -= Duration::seconds(1),
                "head" => pulls[0].head = "b".repeat(40),
                "base" => pulls.iter_mut().for_each(|c| c.base = "c".repeat(40)),
                "membership" => pulls.push(candidate(3, &["a"], Some(1))),
                "gap" => at += Duration::seconds(1),
                "future" => old
                    .values_mut()
                    .for_each(|s| s.last = at.unix_timestamp() + 1),
                _ => (),
            }
            let mut pressure = pressure(pulls);
            if case == "unread" {
                pressure.recovery.get_mut("o/r").unwrap().remove(&2);
            }
            let mut outcome = Outcome::default();
            let next = advance(&pressure, &old, &mut outcome, SCOPE, 72, at);
            assert_eq!(
                outcome.lines.len(),
                usize::from(matches!(case, "at" | "duplicate")),
                "{case}"
            );
            if case == "unread" {
                assert!(next.is_empty());
            } else {
                let seen = next.values().next().unwrap();
                assert_eq!(
                    seen.since,
                    if matches!(case, "at" | "before" | "duplicate") {
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
                assert_eq!(advance(&pressure, &old, &mut same, SCOPE, 72, at), next);
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
