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
use super::*;
use crate::supervise::encode;

/// The most pages of the search one pass reads, 100 pull requests a page.
/// More fails the sweep rather than follow a different part of it each pass.
pub const AUTHORED_PAGES: usize = 3;

/// The most repositories the setting may add a pass.
pub const AUTHORED_LIMIT: usize = 10;

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
