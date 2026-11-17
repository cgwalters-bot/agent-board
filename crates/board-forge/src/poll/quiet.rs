//! `pr.quietWaits`: a followed pull request of the bot whose last whole read
//! left it waiting on the operator alone, a clean midstream draft waiting for
//! promotion ([`PROMOTE_WAIT`]) or a head whose approval the review withheld
//! ([`WITHHELD_WAIT`]), is decided again from the repository's listing while
//! its head and `updated_at` are those of that read, rather than read whole
//! each pass (agent-board#1022). A push, comment, review or label moves
//! `updated_at`, and the next pass reads it whole. What the listing does not
//! show, a base that moved under it or checks rerun at the same head, waits
//! at most [`QUIET_REREAD_SECONDS`] for its next whole read.
//!
//! Only these two waits are kept: other waits on a person depend on other
//! pull requests (an update or a land first), the configuration or a read
//! that failed, none of which moves the listing. The entries come from the
//! trusted state file, as the cached responses do, and carry no text: one
//! forged can only repeat one of these waits, which takes no step.
use super::*;

/// How long a quiet wait stands without a whole read.
pub(crate) const QUIET_REREAD_SECONDS: i64 = 4 * 3600;

/// The most quiet waits the state keeps; past it, a pull request is read
/// whole every pass, as without the option.
pub(crate) const QUIET_LIMIT: usize = 1000;

/// The waits kept, as [`operator_wait`] kinds them, and their reasons.
const QUIET_WAITS: [(&str, &str); 2] = [("promote", PROMOTE_WAIT), ("withheld", WITHHELD_WAIT)];

/// One pull request's last whole read that left it waiting on the operator.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Quiet {
    /// The head and `updated_at` its listing had at that read.
    pub head: String,
    pub updated_at: String,
    /// When it was read whole.
    pub read_at: String,
    /// The [`Wait`] kind, one of [`QUIET_WAITS`].
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_at: Option<String>,
    /// Whether it kept the poll's interval short ([`in_flight`]).
    #[serde(default)]
    pub in_flight: bool,
}

/// `kind` and its reason, if it is one of [`QUIET_WAITS`].
fn quiet_wait(kind: &str) -> Option<(&'static str, &'static str)> {
    QUIET_WAITS.into_iter().find(|(k, _)| *k == kind)
}

/// The state's key of a pull request.
pub(crate) fn key(repo: &str, number: u64) -> String {
    format!("{}#{number}", repo.to_ascii_lowercase())
}

/// The listing's head and `updated_at` of a pull request, where both read.
fn listed(pull: &Value) -> Option<(&str, &str)> {
    let head = pull.pointer("/head/sha").and_then(Value::as_str)?;
    let updated = pull["updated_at"].as_str()?;
    (!head.is_empty() && timestamp(Some(updated)).is_some()).then_some((head, updated))
}

impl Quiet {
    /// What a whole read decided for `listing`'s pull request, if it is one
    /// of [`QUIET_WAITS`] at the listing's head, exactly as [`Self::replay`]
    /// gives it again.
    pub(crate) fn record(
        listing: &Value,
        now: OffsetDateTime,
        read: &(String, bool, Option<Wait>, Option<Stepped>),
    ) -> Option<Self> {
        let (head, updated) = listed(listing)?;
        let (_, _, Some(wait), Some(stepped)) = read else {
            return None;
        };
        let (kind, _) = quiet_wait(wait.kind)?;
        let quiet = Self {
            head: head.to_owned(),
            updated_at: updated.to_owned(),
            read_at: now.format(&Rfc3339).ok()?,
            kind: kind.to_owned(),
            moved_at: stepped.moved_at.and_then(|t| t.format(&Rfc3339).ok()),
            in_flight: read.1,
        };
        let again = quiet.replay(
            (&stepped.repository, stepped.number),
            listing,
            now,
            stepped.ran,
        );
        (again.as_ref() == Some(read)).then_some(quiet)
    }

    /// Whether it still stands at `now`: read within [`QUIET_REREAD_SECONDS`].
    pub(crate) fn fresh(&self, now: OffsetDateTime) -> bool {
        timestamp(Some(&self.read_at))
            .is_some_and(|read| (0..QUIET_REREAD_SECONDS).contains(&(now - read).whole_seconds()))
    }

    /// The decision again, for `listing`'s pull request, if it is unchanged
    /// since and still fresh, as [`Pass::pull`] gives it, its wait taken
    /// (`ran`) as a preview's or an applying pass's is.
    pub(crate) fn replay(
        &self,
        (repo, number): (&str, u64),
        listing: &Value,
        now: OffsetDateTime,
        ran: bool,
    ) -> Option<(String, bool, Option<Wait>, Option<Stepped>)> {
        let (head, updated) = listed(listing)?;
        if head != self.head || updated != self.updated_at || !self.fresh(now) {
            return None;
        }
        let (kind, reason) = quiet_wait(&self.kind)?;
        let wait = Wait {
            repository: repo.to_owned(),
            number,
            head: head.to_owned(),
            kind,
            reason: reason.to_owned(),
            review_bucket: None,
            approval: None,
        };
        let stepped = Stepped {
            repository: repo.to_owned(),
            number,
            head: head.to_owned(),
            action: "wait",
            reason: reason.to_owned(),
            ran,
            moved_at: timestamp(self.moved_at.as_deref()),
            waiting: WaitReason::Person,
        };
        let line = format!("{repo}#{number} {} wait: {reason}", short(head));
        Some((line, self.in_flight, Some(wait), Some(stepped)))
    }

    fn check(&self) -> Result<()> {
        ensure!(
            !self.head.is_empty()
                && self.head.len() <= 64
                && self.head.bytes().all(|b| b.is_ascii_alphanumeric()),
            "head {:?}",
            self.head
        );
        for time in [
            Some(&self.updated_at),
            Some(&self.read_at),
            self.moved_at.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            ensure!(timestamp(Some(time)).is_some(), "time {time:?}");
        }
        ensure!(
            quiet_wait(&self.kind).is_some(),
            "wait kind {:?}",
            self.kind
        );
        Ok(())
    }
}

/// The state's `quiet_waits`, refused whole where any entry is malformed or
/// there are more than [`QUIET_LIMIT`].
pub(crate) fn from_json(value: Option<&Value>) -> Result<BTreeMap<String, Quiet>> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let quiet: BTreeMap<String, Quiet> = serde_json::from_value(value.clone())
        .context("poll state quiet_waits is not a pull-request-to-wait map")?;
    ensure!(
        quiet.len() <= QUIET_LIMIT,
        "poll state quiet_waits has more than {QUIET_LIMIT} entries"
    );
    for (item, entry) in &quiet {
        let valid = item.split_once('#').is_some_and(|(repo, number)| {
            validate_repository(repo).is_ok()
                && *repo == repo.to_ascii_lowercase()
                && number.parse::<u64>().is_ok_and(|n| n > 0)
        });
        ensure!(valid, "poll state quiet_waits item {item:?}");
        entry
            .check()
            .with_context(|| format!("poll state quiet_waits {item}"))?;
    }
    Ok(quiet)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
    const UPDATED: &str = "2026-11-15T10:00:00Z";
    const REPO: &str = "cgwalters-forge/midstream";

    type Read = (String, bool, Option<Wait>, Option<Stepped>);

    fn now() -> OffsetDateTime {
        OffsetDateTime::parse("2026-11-15T12:00:00Z", &Rfc3339).unwrap()
    }

    fn listing() -> Value {
        json!({"number": 7, "head": {"sha": HEAD}, "updated_at": UPDATED})
    }

    /// A whole read's wait of `kind`, if any, for `reason`.
    fn decided(action: &'static str, reason: &str, kind: Option<&'static str>) -> Read {
        let stepped = Stepped {
            repository: REPO.into(),
            number: 7,
            head: HEAD.into(),
            action,
            reason: reason.into(),
            ran: false,
            moved_at: timestamp(Some(UPDATED)),
            waiting: WaitReason::Person,
        };
        let wait = kind.map(|kind| Wait {
            repository: REPO.into(),
            number: 7,
            head: HEAD.into(),
            kind,
            reason: reason.into(),
            review_bucket: None,
            approval: None,
        });
        let line = format!("{REPO}#7 {} {action}: {reason}", short(HEAD));
        (line, true, wait, Some(stepped))
    }

    /// Only a promotion or withheld wait at the listed head, as the listing
    /// gives it again, is kept; it replays as read. Other waits on a person
    /// depend on what the listing does not show.
    #[test]
    fn only_a_wait_on_the_operator_alone_is_kept() {
        let promote = decided("wait", PROMOTE_WAIT, Some("promote"));
        let mut outcome = promote.clone();
        outcome.0.push_str(" -> done");
        let mut pushed = promote.clone();
        pushed.3.as_mut().unwrap().head = "f".repeat(40);
        let mut merge_only = promote.clone();
        merge_only.3 = None;
        let mut followed = promote.clone();
        followed.3.as_mut().unwrap().waiting = WaitReason::Followed { since: None };
        for (name, read, kept) in [
            ("promote", promote.clone(), true),
            (
                "withheld",
                decided("wait", WITHHELD_WAIT, Some("withheld")),
                true,
            ),
            (
                "an update first",
                decided(
                    "wait",
                    "another pull request was brought up to date in this repository this pass",
                    None,
                ),
                false,
            ),
            (
                "an untested draft",
                decided("wait", UNTESTED_WAIT, Some("untested")),
                false,
            ),
            (
                "left to the operator",
                decided("skip", LEFT_TO_OPERATOR, Some("needs-human")),
                false,
            ),
            (
                "approve",
                decided("wait", PROMOTE_WAIT, Some("approve")),
                false,
            ),
            ("a step", decided("fix", PROMOTE_WAIT, None), false),
            ("an outcome", outcome, false),
            ("pushed since listed", pushed, false),
            ("merge only", merge_only, false),
            ("followed", followed, false),
        ] {
            let quiet = Quiet::record(&listing(), now(), &read);
            assert_eq!(quiet.is_some(), kept, "{name}");
            if let Some(quiet) = quiet {
                assert_eq!(
                    quiet.replay((REPO, 7), &listing(), now(), false),
                    Some(read),
                    "{name}"
                );
            }
        }
        assert!(Quiet::record(&json!({"number": 7}), now(), &promote).is_none());
    }

    /// A pull request whose listing moved, or whose read is older than the
    /// bound, is read whole again.
    #[test]
    fn a_changed_or_stale_wait_is_read_again() {
        let promote = decided("wait", PROMOTE_WAIT, Some("promote"));
        let quiet = Quiet::record(&listing(), now(), &promote).unwrap();
        let later = |seconds| now() + time::Duration::seconds(seconds);
        let mut commented = listing();
        commented["updated_at"] = "2026-11-15T11:00:00Z".into();
        let mut pushed = listing();
        pushed["head"]["sha"] = "f".repeat(40).into();
        for (name, listing, at, replays) in [
            ("unchanged", listing(), later(60), true),
            ("commented", commented, later(60), false),
            ("pushed", pushed, later(60), false),
            (
                "last fresh",
                listing(),
                later(QUIET_REREAD_SECONDS - 1),
                true,
            ),
            ("stale", listing(), later(QUIET_REREAD_SECONDS), false),
            ("from the future", listing(), later(-60), false),
        ] {
            assert_eq!(
                quiet.replay((REPO, 7), &listing, at, true).is_some(),
                replays,
                "{name}"
            );
        }
    }

    #[test]
    fn the_state_refuses_malformed_quiet_waits() {
        let promote = decided("wait", PROMOTE_WAIT, Some("promote"));
        let quiet = Quiet::record(&listing(), now(), &promote).unwrap();
        let entry = serde_json::to_value(&quiet).unwrap();
        let item = key(REPO, 7);
        assert_eq!(
            from_json(Some(&json!({&item: entry}))).unwrap(),
            BTreeMap::from([(item.clone(), quiet)])
        );
        let with = |field: &str, value: Value| {
            let mut entry = entry.clone();
            entry[field] = value;
            json!({&item: entry})
        };
        let many: serde_json::Map<String, Value> = (1..=QUIET_LIMIT as u64 + 1)
            .map(|n| (key(REPO, n), entry.clone()))
            .collect();
        for (name, value) in [
            (
                "an upper-case item",
                json!({"Cgwalters-forge/midstream#7": entry}),
            ),
            ("no number", json!({"cgwalters-forge/midstream": entry})),
            ("another kind", with("kind", "needs-human".into())),
            ("a bad time", with("read_at", "yesterday".into())),
            ("a bad head", with("head", "../x".into())),
            ("an unknown field", with("reason", "x".into())),
            ("too many", Value::Object(many)),
        ] {
            assert!(from_json(Some(&value)).is_err(), "{name}");
        }
    }
}
