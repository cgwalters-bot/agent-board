//! Repository backpressure prevents new implements from growing a costly
//! backlog of mutually invalidating bot pull requests. It grants no writes.

use super::*;

pub(crate) const CAP_LIMIT: usize = 100;
const PAGES: usize = 10;
pub(crate) const OPEN_LIMIT: usize = PAGES * 100;
pub(crate) const OLDER_LIMIT: usize = 100;
pub(crate) const FILE_LIMIT: usize = 3000;

/// Refuses an `openPrCap` outside 1 to [`CAP_LIMIT`], in the pass's and the
/// standalone poll's config alike.
pub(crate) fn check_cap(cap: usize) -> Result<()> {
    ensure!(
        (1..=CAP_LIMIT).contains(&cap),
        "openPrCap must be 1 to {CAP_LIMIT}"
    );
    Ok(())
}

fn count(client: &impl Rest, repo: &str, bot: &str) -> Result<usize> {
    let prefix = format!("{API_ROOT}repos/{repo}/pulls?");
    let mut next = Some(format!("{prefix}state=open&per_page=100"));
    let mut open = Vec::new();
    for _ in 0..PAGES {
        let Some(url) = next.take() else { break };
        ensure!(url.starts_with(&prefix), "PR pagination left {repo}");
        let response = client
            .get(&url, None)
            .context("reading open bot PR count")?;
        ensure!(
            response.status == 200,
            "open PR count HTTP {}",
            response.status
        );
        open.extend(
            response
                .body
                .as_array()
                .context("open PRs are not an array")?
                .iter()
                .cloned(),
        );
        next = response.link.as_deref().and_then(poll::next_link);
    }
    ensure!(next.is_none(), "open PR count exceeds {PAGES} pages");
    census(&open, bot)
}

pub(crate) fn census(open: &[Value], bot: &str) -> Result<usize> {
    ensure!(open.len() <= OPEN_LIMIT, "open-pr-cap: too many open PRs");
    let mut seen = BTreeSet::new();
    let mut count = 0;
    for pull in open {
        let number = pull["number"].as_u64().context("PR has no number")?;
        ensure!(
            number > 0 && seen.insert(number),
            "duplicate or invalid PR number"
        );
        let author = pull
            .pointer("/user/login")
            .and_then(Value::as_str)
            .context("PR has no author")?;
        count += usize::from(author.eq_ignore_ascii_case(bot));
    }
    Ok(count)
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    /// What the cap advises for starting a `kind` run in `repo`. It is
    /// advice the coordinator may override: with `past`, the reason it
    /// starts anyway, a start at the cap goes ahead and its start line says
    /// so, with that reason, rather than being held.
    pub(super) fn pr_pressure(&self, (repo, kind): (&str, &str), past: Option<&str>) -> Pressure {
        let Some(cap) = self.config.open_pr_cap.filter(|_| kind == "implement") else {
            return Pressure::Clear;
        };
        let repo = repo.to_ascii_lowercase();
        let mut counts = self.report.open_pr_counts.borrow_mut();
        let count = *counts
            .entry(repo.clone())
            .or_insert_with(|| count(&self.client, &repo, &self.config.bot).ok());
        let (kind, advice) = match count {
            Some(count) if count < cap => return Pressure::Clear,
            Some(count) => (
                HoldKind::OpenPrCap,
                format!("{repo} has {count}/{cap} open bot PRs; landing is the cheaper win"),
            ),
            None => (
                HoldKind::Unreadable,
                format!("{repo} open bot PR count could not be read completely"),
            ),
        };
        match past {
            Some(why) => Pressure::Past(format!(" past open-pr-cap ({advice}): {why}")),
            None => Pressure::Held(Hold::new(
                kind,
                format!("open-pr-cap: {advice}; a plan's past_cap starts it anyway"),
            )),
        }
    }
}

/// [`Scheduler::pr_pressure`]'s advice.
pub(super) enum Pressure {
    Clear,
    /// Starts anyway; the note its line carries, before any ` run=`.
    Past(String),
    Held(Hold),
}

#[cfg(test)]
mod tests {
    use super::*;

    struct List {
        body: Value,
        link: Option<String>,
        status: u16,
        reads: Cell<usize>,
    }

    impl Rest for List {
        fn get(&self, _: &str, _: Option<(&str, &str)>) -> Result<poll::RestResponse> {
            self.reads.set(self.reads.get() + 1);
            Ok(poll::RestResponse {
                status: self.status,
                body: self.body.clone(),
                link: self.link.clone(),
                etag: None,
                last_modified: None,
            })
        }

        fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
            bail!("read only")
        }
    }

    #[test]
    fn open_pr_cap_count_requires_complete_valid_pages() {
        for (body, status, next, expected, reads) in [
            (json!([]), 200, None, Some(0), 1),
            (
                json!([{"number":1,"user":{"login":"BOT"}}, {"number":2,"user":{"login":"other"}}]),
                200,
                None,
                Some(1),
                1,
            ),
            (json!([]), 403, None, None, 1),
            (json!({}), 200, None, None, 1),
            (json!([{"number":1}]), 200, None, None, 1),
            (
                json!([{"number":1,"user":{"login":"bot"}}, {"number":1,"user":{"login":"bot"}}]),
                200,
                None,
                None,
                1,
            ),
            (json!([]), 200, Some("https://example.com/list"), None, 1),
            (
                json!([]),
                200,
                Some("https://api.github.com/repos/other/repo/pulls?page=2"),
                None,
                1,
            ),
            (
                json!([]),
                200,
                Some("https://api.github.com/repos/o/r/pulls?page=2"),
                None,
                PAGES,
            ),
        ] {
            let list = List {
                body,
                status,
                link: next.map(|url| format!("<{url}>; rel=\"next\"")),
                reads: Cell::new(0),
            };
            assert_eq!(count(&list, "o/r", "bot").ok(), expected, "{next:?}");
            assert_eq!(list.reads.get(), reads);
        }
    }
}
