//! `midstream` in the configuration (agent-board#731): the midstream forks,
//! read once a pass from a topic on our organization's repositories rather
//! than listed in `follow` and `pr.noMerge`. Stage 2 follows each and never
//! merges in it, as if it were in both lists.
use super::*;

/// The most pages of the organization's repositories one pass lists. More
/// fails stage 2 rather than leave a midstream unlisted, and so mergeable.
pub const MIDSTREAM_PAGES: usize = 5;

/// GitHub's longest topic.
const TOPIC_LIMIT: usize = 50;

/// The midstream forks' parents, each keyed by the upstream, lower case.
#[derive(Debug, Default)]
pub(super) struct Routes {
    /// Where the intake stages the upstream's direction: the fork, or the
    /// dispatch repository when the fork has issues disabled.
    pub staging: BTreeMap<String, String>,
    /// The midstream fork itself, wherever its direction is staged, so that
    /// an epic naming the fork prioritizes its upstream ([`super::authored`])
    /// and one naming the dispatch repository prioritizes no upstream.
    pub forks: BTreeMap<String, String>,
}

/// Resolve only topic-admitted forks, never a target named in thread text.
/// Ambiguous forks fail closed rather than putting work in the wrong tree.
/// A fork with issues disabled cannot hold the tracking issue staged there,
/// and GitHub refuses that write every pass (HTTP 410): its parent's
/// direction is staged in `fallback`, the dispatch repository, instead.
pub(super) fn routes(
    client: &impl Rest,
    repositories: &[String],
    fallback: &str,
) -> Result<Routes> {
    ensure!(
        repositories.len() <= POLL_REPOSITORY_LIMIT,
        "too many midstream routes"
    );
    let mut routes = Routes::default();
    for repository in repositories {
        let response = client.get(&format!("{API_ROOT}repos/{repository}"), None)?;
        ensure!(response.status == 200, "cannot read midstream {repository}");
        let fork = response.body;
        ensure!(
            text(&fork, "/full_name").eq_ignore_ascii_case(repository),
            "midstream identity changed"
        );
        if fork["fork"] != true {
            continue;
        }
        let parent = text(&fork, "/parent/full_name");
        validate_repository(parent).context("midstream parent")?;
        let staging = match fork["has_issues"] == false {
            true => fallback,
            false => repository,
        };
        let parent = parent.to_ascii_lowercase();
        ensure!(
            (routes.forks)
                .insert(parent.clone(), repository.to_owned())
                .is_none(),
            "multiple midstreams for {parent}"
        );
        routes.staging.insert(parent, staging.to_owned());
    }
    Ok(routes)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Midstream {
    /// The organization whose repositories are listed: `dispatchRepo`'s,
    /// as anyone who can set a topic there chooses what the pass follows.
    pub owner: String,
    /// The topic that makes one of its repositories a midstream.
    pub topic: String,
}

impl Midstream {
    pub(super) fn check(&self, config: &Config) -> Result<()> {
        ensure!(
            is_github_login(&self.owner),
            "midstream owner {:?} is not a login",
            self.owner
        );
        ensure!(
            (config.dispatch_repo.split_once('/'))
                .is_some_and(|(owner, _)| owner.eq_ignore_ascii_case(&self.owner)),
            "midstream owner {} is not dispatchRepo's owner, whose topics only the operator controls",
            self.owner
        );
        let topic = &self.topic;
        ensure!(
            (1..=TOPIC_LIMIT).contains(&topic.len())
                && !topic.starts_with('-')
                && topic
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "midstream topic {topic:?} is not a GitHub topic"
        );
        Ok(())
    }

    /// The owner's repositories with the topic, sorted by lower-case name.
    pub(super) fn read(&self, client: &impl Rest) -> Result<Vec<String>> {
        let owner = &self.owner;
        let mut next = Some(format!("{API_ROOT}orgs/{owner}/repos?per_page=100"));
        let mut found = BTreeMap::new();
        for _ in 0..MIDSTREAM_PAGES {
            let Some(url) = next.take() else { break };
            let response = client.get(&url, None)?;
            ensure!(
                response.status == 200,
                "GET {url}: GitHub HTTP status {}",
                response.status
            );
            let repositories = (response.body.as_array()).context("repositories are not a list")?;
            for repository in repositories {
                let name = repository["full_name"]
                    .as_str()
                    .context("a repository has no full_name")?;
                let topics = repository["topics"]
                    .as_array()
                    .with_context(|| format!("{name} has no topics"))?;
                if topics.iter().any(|t| t == self.topic.as_str()) {
                    validate_repository(name)?;
                    ensure!(
                        name.split_once('/')
                            .is_some_and(|(o, _)| o.eq_ignore_ascii_case(owner)),
                        "{name} is listed as {owner}'s"
                    );
                    found.insert(name.to_ascii_lowercase(), name.to_owned());
                }
            }
            next = response.link.as_deref().and_then(next_link);
            // The token goes wherever the next page is.
            if let Some(url) = next.as_ref().filter(|u| !u.starts_with(API_ROOT)) {
                bail!("the next page {url} is outside {API_ROOT}");
            }
        }
        ensure!(
            next.is_none(),
            "{owner} has more than {MIDSTREAM_PAGES} pages of repositories"
        );
        Ok(found.into_values().collect())
    }
}
