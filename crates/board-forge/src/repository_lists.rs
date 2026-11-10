//! Trusted GitHub repository-list sources, expanded before policy validation.
use crate::github::{API_ROOT, is_github_login};
use crate::poll::{Rest, validate_repository};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// Normalizing JSON must not turn duplicate scope fields (for example two
/// dispatchRepo keys) into a silently selected last value. Validate the source
/// document before its map representation can discard duplicates.
pub(crate) fn check_source_fields(text: &str) -> Result<()> {
    struct Unique;

    impl<'de> Deserialize<'de> for Unique {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Fields;

            impl<'de> serde::de::Visitor<'de> for Fields {
                type Value = Unique;

                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("JSON without duplicate configuration fields")
                }

                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> Result<Unique, A::Error> {
                    let mut seen = BTreeSet::new();
                    while let Some(key) = map.next_key::<String>()? {
                        if !seen.insert(key.clone()) {
                            return Err(serde::de::Error::custom(format!(
                                "duplicate configuration field {key}"
                            )));
                        }
                        map.next_value::<Unique>()?;
                    }
                    Ok(Unique)
                }

                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut seq: A,
                ) -> Result<Unique, A::Error> {
                    while seq.next_element::<Unique>()?.is_some() {}
                    Ok(Unique)
                }

                fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Unique, E> {
                    Ok(Unique)
                }

                fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Unique, E> {
                    Ok(Unique)
                }

                fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Unique, E> {
                    Ok(Unique)
                }

                fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Unique, E> {
                    Ok(Unique)
                }

                fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Unique, E> {
                    Ok(Unique)
                }

                fn visit_unit<E: serde::de::Error>(self) -> Result<Unique, E> {
                    Ok(Unique)
                }
            }

            deserializer.deserialize_any(Fields)
        }
    }

    serde_json::from_str::<Unique>(text).context("pass source fields")?;
    Ok(())
}

/// Prevent a configured organization from turning discovery into an unbounded
/// read. Incomplete inventories fail closed, never silently narrow policy.
const ORG_LIMIT: usize = 10;
const PAGE_LIMIT: usize = 10;
const PAGE_SIZE: usize = 100;
pub const RESOLUTION_SECONDS: u64 = 120;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Organization {
    org: String,
    forks: bool,
}

/// Original lists containing organizations, indexed by JSON pointer. Runtime
/// policy still contains only concrete repository names.
#[derive(Debug, Default)]
pub struct Sources(BTreeMap<String, Vec<Value>>);

impl Sources {
    /// Extract sources only from repository-list keys, not arbitrary arrays
    /// (briefs, labels, pools, paths or untrusted embedded documents).
    pub fn take(value: &mut Value) -> Result<Self> {
        fn visit(value: &mut Value, path: &str, sources: &mut Sources) -> Result<()> {
            if let Some(array) = value.as_array_mut() {
                for (index, entry) in array.iter_mut().enumerate() {
                    visit(entry, &format!("{path}/{index}"), sources)?;
                }
                return Ok(());
            }
            let Some(object) = value.as_object_mut() else {
                return Ok(());
            };
            for (key, value) in object {
                let path = format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"));
                if matches!(
                    key.as_str(),
                    "follow"
                        | "workRepositories"
                        | "repositories"
                        | "runRepositories"
                        | "harnessRepositories"
                        | "reviewDispatchRepositories"
                        | "draftReviewRepositories"
                        | "noMerge"
                ) || matches!(
                    path.as_str(),
                    "/maintain/repos/include" | "/maintain/repos/exclude" | "/review/targets"
                ) {
                    if let Some(list) = value.as_array_mut()
                        && list.iter().any(Value::is_object)
                    {
                        for entry in list.iter() {
                            if entry.is_object() {
                                let org: Organization = serde_json::from_value(entry.clone())
                                    .with_context(|| format!("{path}: organization entry"))?;
                                ensure!(is_github_login(&org.org), "{path}: invalid organization");
                                ensure!(
                                    !org.forks,
                                    "{path}: organization entries require forks: false"
                                );
                            } else {
                                entry
                                    .as_str()
                                    .context("repository entry must be a string or organization")?;
                            }
                        }
                        sources.0.insert(path, list.clone());
                        list.retain(Value::is_string);
                    }
                } else {
                    visit(value, &path, sources)?;
                }
            }
            Ok(())
        }
        let mut sources = Self::default();
        visit(value, "", &mut sources)?;
        let orgs: BTreeSet<_> = sources
            .0
            .values()
            .flatten()
            .filter_map(|e| e["org"].as_str())
            .map(str::to_ascii_lowercase)
            .collect();
        ensure!(
            orgs.len() <= ORG_LIMIT,
            "repository lists name more than {ORG_LIMIT} organizations"
        );
        Ok(sources)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn resolve(
        &self,
        value: &mut Value,
        client: &impl Rest,
        lines: &mut Vec<String>,
    ) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(RESOLUTION_SECONDS);
        let mut cache = BTreeMap::new();
        for (path, entries) in &self.0 {
            let mut resolved = BTreeMap::new();
            for entry in entries {
                if let Some(repo) = entry.as_str() {
                    resolved
                        .entry(repo.to_ascii_lowercase())
                        .or_insert_with(|| repo.to_owned());
                } else {
                    let org = entry["org"]
                        .as_str()
                        .context("organization missing")?
                        .to_ascii_lowercase();
                    if !cache.contains_key(&org) {
                        let repos = listing(client, &org, deadline)?;
                        lines.push(format!(
                            "repositories: org {org} resolved [{}]",
                            repos.join(", ")
                        ));
                        cache.insert(org.clone(), repos);
                    }
                    for repo in &cache[&org] {
                        resolved
                            .entry(repo.to_ascii_lowercase())
                            .or_insert_with(|| repo.clone());
                    }
                }
            }
            *value
                .pointer_mut(path)
                .context("repository list disappeared")? =
                json!(resolved.into_values().collect::<Vec<_>>());
            lines.push(format!(
                "repositories: {path} resolved {}",
                value.pointer(path).context("repository list disappeared")?
            ));
        }
        Ok(())
    }
}

fn listing(client: &impl Rest, org: &str, deadline: Instant) -> Result<Vec<String>> {
    let mut repos = BTreeMap::new();
    for page in 1..=PAGE_LIMIT {
        ensure!(
            Instant::now() < deadline,
            "organization resolution deadline reached"
        );
        let url = format!(
            "{API_ROOT}orgs/{org}/repos?type=all&sort=full_name&per_page={PAGE_SIZE}&page={page}"
        );
        let response = client
            .get(&url, None)
            .with_context(|| format!("list organization {org}"))?;
        ensure!(
            response.status == 200,
            "organization {org} listing returned {}",
            response.status
        );
        let values = response
            .body
            .as_array()
            .context("organization listing is not an array")?;
        ensure!(
            values.len() <= PAGE_SIZE,
            "organization listing exceeds page size"
        );
        for value in values {
            let name = value["full_name"]
                .as_str()
                .context("org repository missing full_name")?;
            validate_repository(name)?;
            ensure!(
                name.split_once('/')
                    .is_some_and(|(owner, _)| owner.eq_ignore_ascii_case(org)),
                "organization listing contains foreign repository {name}"
            );
            let fork = value["fork"]
                .as_bool()
                .context("org repository missing fork")?;
            let archived = value["archived"]
                .as_bool()
                .context("org repository missing archived")?;
            if !fork && !archived {
                repos.insert(name.to_ascii_lowercase(), name.to_owned());
            }
        }
        if values.len() < PAGE_SIZE
            && !response
                .link
                .as_deref()
                .is_some_and(|link| link.contains("rel=\"next\""))
        {
            return Ok(repos.into_values().collect());
        }
    }
    anyhow::bail!(
        "organization {org} listing exceeds {PAGE_LIMIT} pages; refusing incomplete repository set"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pass::Replay;
    use crate::poll::RestResponse;
    use std::cell::Cell;

    // Recorded 2026-11-10, projected to the three fields resolution uses.
    const RECORDING: &str = include_str!("../../../fixtures/pass/org-repositories.json");

    #[test]
    fn recorded_org_lists_are_additive_and_shared_across_fields() {
        let replay = Replay::from_json(&serde_json::from_str(RECORDING).unwrap()).unwrap();
        let recorder = crate::pass::Recorder::new(&replay);
        let org = json!({"org":"cgwalters-forge", "forks":false});
        let mut value = json!({
            "follow": [org, "external/upstream", "CGWALTERS-FORGE/actions"],
            "workRepositories": [org],
            "sessions": {"repositories": [org]},
            "holds": {"proposals": {"repositories": [org]}},
            "operatorAsk": {"harness": {"repositories": [org]}},
            "budget": {"harnessRepositories": [org]},
            "pr": {"noMerge": [org], "reviewDispatchRepositories": [org]},
            "labels": [org]
        });
        let sources = Sources::take(&mut value).unwrap();
        let mut lines = Vec::new();
        sources.resolve(&mut value, &recorder, &mut lines).unwrap();
        let recorded: Value = serde_json::from_str(&recorder.fixture(replay.now).unwrap()).unwrap();
        assert_eq!(recorded["requests"].as_array().unwrap().len(), 1);
        let projected = crate::promote::recording(&recorded.to_string()).unwrap();
        let projected = Replay::from_json(&serde_json::from_str(&projected).unwrap()).unwrap();
        sources
            .resolve(&mut value, &projected, &mut Vec::new())
            .unwrap();
        let expected = value["workRepositories"].as_array().unwrap();
        assert_eq!(expected.len(), 14);
        assert!(expected.contains(&json!("cgwalters-forge/agent-board")));
        assert!(!expected.contains(&json!("cgwalters-forge/bootc")));
        assert_eq!(
            value["follow"].as_array().unwrap().len(),
            expected.len() + 1
        );
        for path in [
            "/sessions/repositories",
            "/holds/proposals/repositories",
            "/operatorAsk/harness/repositories",
            "/budget/harnessRepositories",
            "/pr/noMerge",
            "/pr/reviewDispatchRepositories",
        ] {
            assert_eq!(value.pointer(path).unwrap(), &json!(expected));
        }
        assert_eq!(value["labels"], json!([org]));
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.starts_with("repositories: org "))
                .count(),
            1
        );
    }

    struct Pages {
        calls: Cell<usize>,
        body: Value,
        link: Option<String>,
        status: u16,
    }

    impl Rest for Pages {
        fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
            let page = self.calls.get() + 1;
            assert!(url.ends_with(&format!("page={page}")));
            self.calls.set(page);
            Ok(RestResponse {
                status: self.status,
                etag: None,
                last_modified: None,
                link: self.link.clone(),
                body: self.body.clone(),
            })
        }

        fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
            panic!("repository resolution must not write")
        }
    }

    #[test]
    fn organization_inventory_fails_closed_and_is_bounded() {
        for (body, status, link, calls) in [
            (json!([]), 403, None, 1),
            (json!({}), 200, None, 1),
            (
                json!([{"full_name":"other/repo","fork":false,"archived":false}]),
                200,
                None,
                1,
            ),
            (
                json!([{"full_name":"team/repo","archived":false}]),
                200,
                None,
                1,
            ),
            (
                json!([{"full_name":"team/repo","fork":false}]),
                200,
                None,
                1,
            ),
            (
                json!([]),
                200,
                Some("<https://evil.invalid/>; rel=\"next\"".into()),
                PAGE_LIMIT,
            ),
        ] {
            let pages = Pages {
                calls: Cell::new(0),
                body,
                link,
                status,
            };
            assert!(
                listing(
                    &pages,
                    "team",
                    Instant::now() + Duration::from_secs(RESOLUTION_SECONDS)
                )
                .is_err()
            );
            assert_eq!(pages.calls.get(), calls);
        }
    }

    #[test]
    fn archived_and_fork_repositories_are_excluded_but_explicit_entries_remain() {
        let pages = Pages {
            calls: Cell::new(0),
            status: 200,
            link: None,
            body: json!([
                {"full_name":"team/old","fork":false,"archived":true},
                {"full_name":"team/fork","fork":true,"archived":false}
            ]),
        };
        let mut value = json!({"follow":[{"org":"team","forks":false}, "team/fork"]});
        let sources = Sources::take(&mut value).unwrap();
        sources
            .resolve(&mut value, &pages, &mut Vec::new())
            .unwrap();
        assert_eq!(value["follow"], json!(["team/fork"]));
    }

    #[test]
    fn invalid_sources_fail_before_any_read() {
        for entry in [
            json!({"org":"../team","forks":false}),
            json!({"org":"team","forks":true}),
            json!({"org":"team"}),
            json!({"org":"team","forks":false,"extra":true}),
            json!({"org":null,"forks":false}),
        ] {
            assert!(Sources::take(&mut json!({"follow":[entry]})).is_err());
        }
    }

    fn source_config() -> Value {
        json!({"operator":"operator", "bot":"bot", "dispatchRepo":"team/dispatch",
            "dispatchWorkflow":"dispatch.yml", "dispatchRef":"main",
            "workRepositories":[{"org":"cgwalters-forge","forks":false}]})
    }

    #[tokio::test]
    async fn pass_resolves_before_stages_and_counts_the_listing() {
        use crate::pass::{self, Config, Mode};
        let config = Config::parse_sources(&source_config().to_string()).unwrap();
        let replay = Replay::from_json(&serde_json::from_str(RECORDING).unwrap()).unwrap();
        let report = pass::run(
            &replay,
            &replay,
            &config,
            Default::default(),
            (Mode::Preview, Default::default()),
            replay.now,
        )
        .await
        .unwrap();
        assert_eq!(report.failures, 0, "{:?}", report.lines);
        assert_eq!(report.spent.requests, 1);
        assert!(report.lines[0].starts_with("repositories: org cgwalters-forge resolved ["));
        assert!(
            config.has_repository_sources(),
            "do not cache one pass's resolved inventory in the source"
        );
    }

    #[test]
    fn resolved_policy_is_revalidated_and_pool_order_is_preserved() {
        use crate::pass::Config;
        let mut value = source_config();
        // Deliberately not alphabetical: pool order breaks scheduling ties.
        let text = value.to_string().replacen("{", "{\"pools\":{\"zeta\":{},\"alpha\":{}},\"usage\":{\"repository\":\"team/usage\",\"issue\":1},", 1);
        let config = Config::parse_sources(&text).unwrap();
        let replay = Replay::from_json(&serde_json::from_str(RECORDING).unwrap()).unwrap();
        let resolved = config.resolve(&replay, &mut Vec::new()).unwrap().unwrap();
        assert_eq!(
            resolved
                .pools
                .0
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>(),
            ["zeta", "alpha"]
        );
        assert!(Config::parse_sources(&text.replace("\"alpha\"", "\"zeta\"")).is_err());

        value["wip"] = json!(1); // No board: invalid only after work sources are known.
        let config = Config::parse_sources(&value.to_string()).unwrap();
        assert!(
            config
                .resolve(&replay, &mut Vec::new())
                .unwrap_err()
                .to_string()
                .contains("board is required")
        );
        assert!(
            Config::parse(&value.to_string()).is_err(),
            "concrete-only callers must not silently omit organizations"
        );
    }

    #[test]
    fn source_inventory_is_fresh_on_each_resolution() {
        let mut value = source_config();
        let sources = Sources::take(&mut value).unwrap();
        for name in ["old", "new"] {
            let pages = Pages {
                calls: Cell::new(0),
                status: 200,
                link: None,
                body: json!([{"full_name":format!("cgwalters-forge/{name}"),"fork":false,"archived":false}]),
            };
            sources
                .resolve(&mut value, &pages, &mut Vec::new())
                .unwrap();
            assert_eq!(
                value["workRepositories"],
                json!([format!("cgwalters-forge/{name}")])
            );
        }
    }

    #[test]
    fn pagination_exhausts_full_pages_and_short_pages_with_a_next_link() {
        for first_size in [1, PAGE_SIZE] {
            let mut fixture: Value = serde_json::from_str(RECORDING).unwrap();
            let mut first = fixture["requests"][0].clone();
            first["answer"]["body"] = json!(
                (0..first_size)
                    .map(|n| json!({
                        "full_name": format!("cgwalters-forge/repo{n:03}"),
                        "fork": false, "archived": false
                    }))
                    .collect::<Vec<_>>()
            );
            // The header URL is deliberately not an authority for the next GET.
            first["answer"]["link"] = json!("<https://other.invalid/anything>; rel=\"next\"");
            let mut second = first.clone();
            second["request"]["get"] = json!(format!(
                "{API_ROOT}orgs/cgwalters-forge/repos?type=all&sort=full_name&per_page={PAGE_SIZE}&page=2"
            ));
            second["answer"]["link"] = Value::Null;
            second["answer"]["body"] =
                json!([{"full_name":"cgwalters-forge/last","fork":false,"archived":false}]);
            fixture["requests"] = json!([first, second]);
            let replay = Replay::from_json(&fixture).unwrap();
            let mut value = source_config();
            let sources = Sources::take(&mut value).unwrap();
            sources
                .resolve(&mut value, &replay, &mut Vec::new())
                .unwrap();
            assert_eq!(
                value["workRepositories"].as_array().unwrap().len(),
                first_size + 1
            );
        }
    }

    #[test]
    fn organization_count_and_deadline_are_bounded() {
        let entries: Vec<_> = (0..=ORG_LIMIT)
            .map(|n| json!({"org":format!("team{n}"),"forks":false}))
            .collect();
        assert!(Sources::take(&mut json!({"follow":entries})).is_err());
        let pages = Pages {
            calls: Cell::new(0),
            status: 200,
            body: json!([]),
            link: None,
        };
        assert!(listing(&pages, "team", Instant::now()).is_err());
        assert_eq!(pages.calls.get(), 0);
    }

    #[test]
    fn source_normalization_cannot_hide_duplicate_policy_fields() {
        let text = source_config().to_string();
        for extra in [
            "\"dispatchRepo\":\"other/dispatch\",",
            "\"board\":{\"owner\":\"one\",\"owner\":\"two\",\"project\":1},",
            "\"pr\":{\"unstall\":false,\"unstall\":true},",
        ] {
            let text = format!("{{{extra}{}", &text[1..]);
            let error = crate::pass::Config::parse_sources(&text).unwrap_err();
            assert!(
                format!("{error:#}").contains("duplicate configuration field"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn status_repository_and_run_repository_lists_use_the_same_resolution() {
        let replay = Replay::from_json(&serde_json::from_str(RECORDING).unwrap()).unwrap();
        let mut value = json!({"statusReconcile": {
            "repositories":[{"org":"cgwalters-forge","forks":false}],
            "runRepositories":[{"org":"cgwalters-forge","forks":false}, "external/runs"]
        }});
        let sources = Sources::take(&mut value).unwrap();
        sources
            .resolve(&mut value, &replay, &mut Vec::new())
            .unwrap();
        let config = crate::status::configured(&value).unwrap();
        assert_eq!(config.repositories.len(), 14);
        assert_eq!(config.run_repositories.len(), 15);
        assert!(
            config
                .run_repositories
                .contains(&"external/runs".to_owned())
        );
    }
}
