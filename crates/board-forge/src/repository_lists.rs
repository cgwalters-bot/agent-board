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
    #[serde(default)]
    visibility: Visibility,
}

// Private repositories must be named explicitly, never enrolled by discovery.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Visibility {
    #[default]
    Public,
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
                                let Visibility::Public = org.visibility;
                                if org.forks {
                                    ensure!(
                                        matches!(
                                            path.as_str(),
                                            "/follow" | "/workRepositories" | "/pr/noMerge"
                                        ),
                                        "{path}: forks: true is supported only in follow, workRepositories and pr.noMerge"
                                    );
                                }
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
    ) -> Result<Option<Vec<String>>> {
        let deadline = Instant::now() + Duration::from_secs(RESOLUTION_SECONDS);
        let mut cache = BTreeMap::new();
        let mut inventories = BTreeMap::new();
        let mut midstreams = BTreeMap::new();
        let metadata = self
            .0
            .values()
            .flatten()
            .any(|entry| entry["forks"] == true);
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
                        let inventory = inventory(client, &org, deadline)?;
                        let repos = owned_listing(&inventory);
                        if !metadata {
                            lines.push(format!(
                                "repositories: org {org} resolved [{}]",
                                repos.join(", ")
                            ));
                        }
                        inventories.insert(org.clone(), inventory);
                        cache.insert(org.clone(), repos);
                    }
                    let forks = entry["forks"] == true;
                    let key = format!("{org}/metadata");
                    if forks && !cache.contains_key(&key) {
                        ensure!(
                            value["midstream"]["owner"]
                                .as_str()
                                .is_some_and(|owner| owner.eq_ignore_ascii_case(&org)),
                            "organization {org} with forks: true requires matching midstream.owner"
                        );
                        let topic = value["midstream"]["topic"]
                            .as_str()
                            .context("midstream.topic missing")?;
                        // Public discovery must not revoke admission for an
                        // explicitly named private midstream. Read only exact
                        // caller names missing from the public inventory.
                        let mut explicit: BTreeSet<_> = value["follow"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .chain(
                                value["promote"]["forks"]
                                    .as_object()
                                    .into_iter()
                                    .flatten()
                                    .map(|(name, _)| name.as_str()),
                            )
                            .filter(|name| {
                                name.split_once('/')
                                    .is_some_and(|(owner, _)| owner.eq_ignore_ascii_case(&org))
                            })
                            .map(str::to_ascii_lowercase)
                            .collect();
                        let inventory = inventories
                            .get_mut(&org)
                            .context("organization inventory disappeared")?;
                        // A legacy owned source can retain a detached staged
                        // repository (GitHub fork=false, origin=fork). Treat
                        // its effective follows like literal names below.
                        if self.0.get("/follow").is_some_and(|entries| {
                            entries.iter().any(|entry| {
                                entry["forks"] == false
                                    && entry["org"]
                                        .as_str()
                                        .is_some_and(|name| name.eq_ignore_ascii_case(&org))
                            })
                        }) {
                            explicit.extend(
                                owned_listing(inventory)
                                    .into_iter()
                                    .map(|name| name.to_ascii_lowercase()),
                            );
                        }
                        for name in &explicit {
                            if inventory.iter().any(|v| {
                                v["full_name"]
                                    .as_str()
                                    .is_some_and(|v| v.eq_ignore_ascii_case(name))
                            }) {
                                continue;
                            }
                            validate_repository(name)?;
                            ensure!(
                                Instant::now() < deadline,
                                "organization resolution deadline reached"
                            );
                            let response = client
                                .get(&format!("{API_ROOT}repos/{name}"), None)
                                .with_context(|| format!("read explicit midstream {name}"))?;
                            ensure!(
                                response.status == 200,
                                "{name}: explicit metadata returned {}",
                                response.status
                            );
                            ensure!(
                                response.body["full_name"]
                                    .as_str()
                                    .is_some_and(|v| v.eq_ignore_ascii_case(name)),
                                "{name}: explicit metadata identity changed"
                            );
                            classification(&response.body)?;
                            properties(&response.body)?;
                            // Do not enrol an explicitly named owned private
                            // repository in any organization-backed list.
                            if response.body["private"] == true
                                && response.body["custom_properties"]["origin"] == "fork"
                            {
                                inventory.push(response.body);
                            }
                        }
                        let (repos, mappings) = metadata_listing(
                            client, &org, topic, inventory, &explicit, deadline, lines,
                        )?;
                        midstreams.extend(mappings);
                        cache.insert(key.clone(), repos);
                    }
                    let repos = &cache[if forks { &key } else { &org }];
                    for repo in repos {
                        if forks
                            && path == "/pr/noMerge"
                            && !midstreams.contains_key(&repo.to_ascii_lowercase())
                        {
                            continue;
                        }
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
                "repositories: {}{path} resolved {}",
                if metadata { "detail " } else { "" },
                value.pointer(path).context("repository list disappeared")?
            ));
        }
        if metadata {
            // Enrolment and no-merge travel together: discovering a fork must
            // never accidentally authorize merging staged work into it.
            for path in ["follow", "workRepositories"] {
                append(
                    value,
                    path,
                    midstreams.values().map(|(name, _)| name.clone()),
                )?;
            }
            if value.get("pr").is_none() {
                value["pr"] = json!({});
            }
            append(
                &mut value["pr"],
                "noMerge",
                midstreams.values().map(|(name, _)| name.clone()),
            )?;
            if !midstreams.is_empty() {
                if value.get("promote").is_none() {
                    value["promote"] = json!({});
                }
                if value["promote"].get("forks").is_none() {
                    value["promote"]["forks"] = json!({});
                }
                let explicit = value["promote"]["forks"]
                    .as_object_mut()
                    .context("promote.forks must be an object")?;
                for (key, (name, mapping)) in &midstreams {
                    if !explicit.keys().any(|name| name.eq_ignore_ascii_case(key)) {
                        explicit.insert(name.clone(), mapping.clone());
                    }
                }
            }
            Ok(Some(
                midstreams.into_values().map(|(name, _)| name).collect(),
            ))
        } else {
            Ok(None)
        }
    }
}

fn append(value: &mut Value, key: &str, names: impl Iterator<Item = String>) -> Result<()> {
    if value.get(key).is_none() {
        value[key] = json!([]);
    }
    let list = value[key]
        .as_array_mut()
        .with_context(|| format!("{key} must be a list"))?;
    for name in names {
        if !list
            .iter()
            .any(|v| v.as_str().is_some_and(|v| v.eq_ignore_ascii_case(&name)))
        {
            list.push(json!(name));
        }
    }
    list.sort_by_key(|v| v.as_str().unwrap_or_default().to_ascii_lowercase());
    Ok(())
}

type Mappings = BTreeMap<String, (String, Value)>;

fn classification(value: &Value) -> Result<()> {
    for field in ["fork", "archived", "private"] {
        value[field].as_bool().with_context(|| {
            format!(
                "repository {}: missing or invalid {field}",
                value["full_name"]
            )
        })?;
    }
    Ok(())
}

fn properties(value: &Value) -> Result<&serde_json::Map<String, Value>> {
    let name = &value["full_name"];
    let properties = value["custom_properties"]
        .as_object()
        .with_context(|| format!("{name}: custom_properties missing or invalid"))?;
    ensure!(
        properties.get("origin").is_none_or(Value::is_string),
        "{name}: invalid origin property"
    );
    Ok(properties)
}

fn metadata_listing(
    client: &impl Rest,
    org: &str,
    topic: &str,
    inventory: &[Value],
    explicit: &BTreeSet<String>,
    deadline: Instant,
    lines: &mut Vec<String>,
) -> Result<(Vec<String>, Mappings)> {
    let mut repos = Vec::new();
    let mut mappings = BTreeMap::new();
    let mut excluded = 0;
    let mut owned = 0;
    for value in inventory {
        let name = value["full_name"]
            .as_str()
            .context("repository name missing")?;
        let reason;
        if value["archived"] == true
            || (value["private"] == true && !explicit.contains(&name.to_ascii_lowercase()))
        {
            reason = "archived or private";
        } else {
            let properties = properties(value)?;
            if properties
                .get("origin")
                .is_some_and(|origin| origin == "fork")
            {
                let topics = value["topics"]
                    .as_array()
                    .with_context(|| format!("{name}: topics missing"))?;
                ensure!(
                    topics.iter().all(Value::is_string),
                    "{name}: invalid topics"
                );
                if topics.iter().any(|t| t == topic) {
                    ensure!(
                        Instant::now() < deadline,
                        "organization resolution deadline reached"
                    );
                    let response = client
                        .get(&format!("{API_ROOT}repos/{name}"), None)
                        .with_context(|| format!("read midstream parent {name}"))?;
                    ensure!(
                        response.status == 200,
                        "{name}: metadata returned {}",
                        response.status
                    );
                    ensure!(
                        response.body["full_name"]
                            .as_str()
                            .is_some_and(|v| v.eq_ignore_ascii_case(name)),
                        "{name}: metadata identity changed"
                    );
                    if response.body["parent"].is_null() {
                        // An explicit follow survives discovery exclusions.
                        // Refuse the configuration rather than let that staged
                        // repository reach the poll without no-merge protection.
                        ensure!(
                            !explicit.contains(&name.to_ascii_lowercase()),
                            "{name}: followed midstream parent missing"
                        );
                        lines.push(format!(
                            "repositories: refused {name}: midstream parent missing"
                        ));
                        reason = "midstream parent missing";
                    } else {
                        ensure!(
                            response.body["fork"] == true,
                            "{name}: origin fork has no GitHub fork parent"
                        );
                        let parent = response.body["parent"]["full_name"]
                            .as_str()
                            .with_context(|| format!("{name}: parent name missing"))?;
                        validate_repository(parent)?;
                        let base = response.body["parent"]["default_branch"]
                            .as_str()
                            .filter(|v| !v.trim().is_empty())
                            .with_context(|| format!("{name}: parent default branch missing"))?;
                        mappings.insert(
                            name.to_ascii_lowercase(),
                            (name.to_owned(), json!({"upstream":parent,"base":base})),
                        );
                        repos.push(name.to_owned());
                        lines.push(format!("repositories: detail {name}: midstream (origin fork, topic {topic}), parent {parent}, base {base}"));
                        continue;
                    }
                } else {
                    reason = "fork without midstream topic";
                }
            } else {
                owned += 1;
                repos.push(name.to_owned());
                lines.push(format!(
                    "repositories: detail {name}: owned (origin is not fork)"
                ));
                continue;
            }
        }
        excluded += 1;
        lines.push(format!("repositories: detail {name}: excluded ({reason})"));
    }
    lines.push(format!(
        "repositories: metadata {org}: {owned} owned, {} midstream, {excluded} excluded",
        mappings.len()
    ));
    Ok((repos, mappings))
}

#[cfg(test)]
fn listing(client: &impl Rest, org: &str, deadline: Instant) -> Result<Vec<String>> {
    Ok(owned_listing(&inventory(client, org, deadline)?))
}

fn owned_listing(inventory: &[Value]) -> Vec<String> {
    inventory
        .iter()
        .filter(|v| v["fork"] == false && v["archived"] == false && v["private"] == false)
        .filter_map(|v| v["full_name"].as_str().map(str::to_owned))
        .collect()
}

fn inventory(client: &impl Rest, org: &str, deadline: Instant) -> Result<Vec<Value>> {
    let mut repos = BTreeMap::new();
    for page in 1..=PAGE_LIMIT {
        ensure!(
            Instant::now() < deadline,
            "organization resolution deadline reached"
        );
        let url = format!(
            "{API_ROOT}orgs/{org}/repos?type=public&sort=full_name&per_page={PAGE_SIZE}&page={page}"
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
            classification(value)?;
            repos.insert(name.to_ascii_lowercase(), value.clone());
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

    // Recorded 2026-11-10, projected to the fields resolution uses.
    const RECORDING: &str = include_str!("../../../fixtures/pass/org-repositories.json");

    #[test]
    fn metadata_rules_and_explicit_overrides() {
        for (case, origin, topics, archived, parent, explicit, included, promoted) in [
            (
                "owned",
                "upstream",
                json!([]),
                false,
                true,
                false,
                true,
                false,
            ),
            (
                "midstream",
                "fork",
                json!(["bot-midstream"]),
                false,
                true,
                false,
                true,
                true,
            ),
            (
                "unselected fork",
                "fork",
                json!([]),
                false,
                true,
                false,
                false,
                false,
            ),
            (
                "archived",
                "fork",
                json!(["bot-midstream"]),
                true,
                true,
                false,
                false,
                false,
            ),
            (
                "explicit override",
                "fork",
                json!(["bot-midstream"]),
                false,
                true,
                true,
                true,
                true,
            ),
            (
                "explicit excluded",
                "fork",
                json!([]),
                false,
                true,
                true,
                true,
                false,
            ),
            (
                "missing parent",
                "fork",
                json!(["bot-midstream"]),
                false,
                false,
                false,
                false,
                false,
            ),
            (
                "explicit missing parent",
                "fork",
                json!(["bot-midstream"]),
                false,
                false,
                true,
                false,
                false,
            ),
            (
                "mixed sources missing parent",
                "fork",
                json!(["bot-midstream"]),
                false,
                false,
                false,
                false,
                false,
            ),
        ] {
            let repo = json!({"full_name":"team/repo", "fork":origin == "fork" && parent, "archived":archived,
                "private":false,"custom_properties":{"origin":origin},"topics":topics});
            let fixture = json!({"schema":"agent-board/pass-replay/v1","now":"2026-11-10T00:00:00Z", "requests":[
                {"request":{"get":format!("{API_ROOT}orgs/team/repos?type=public&sort=full_name&per_page=100&page=1")},
                 "answer":{"status":200,"body":[repo]}},
                {"request":{"get":format!("{API_ROOT}repos/team/repo")},
                 "answer":{"status":200,"body":{"full_name":"team/repo","fork":parent,
                    "parent":if parent {json!({"full_name":"up/repo","default_branch":"trunk"})} else {Value::Null}}}}
            ]});
            let replay = Replay::from_json(&fixture).unwrap();
            let recorder = crate::pass::Recorder::new(&replay);
            let org = json!({"org":"team","forks":true});
            let mut value = json!({"midstream":{"owner":"team","topic":"bot-midstream"},
                "budget":{"harnessRepositories":[{"org":"team","forks":false}]},
                "follow":[org],"workRepositories":[org],"pr":{"noMerge":[org]}});
            if explicit {
                value["follow"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("TEAM/repo"));
                value["promote"] =
                    json!({"forks":{"TEAM/repo":{"upstream":"other/repo","base":"stable"}}});
            }
            if case == "mixed sources missing parent" {
                value["follow"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"org":"team","forks":false}));
            }
            if case == "missing parent" {
                let mut config_value = value.clone();
                config_value["operator"] = json!("operator");
                config_value["bot"] = json!("bot");
                config_value["dispatchRepo"] = json!("team/dispatch");
                config_value["dispatchWorkflow"] = json!("dispatch.yml");
                let config = crate::pass::Config::parse_sources(&config_value.to_string()).unwrap();
                let resolved = config.resolve(&replay, &mut Vec::new()).unwrap().unwrap();
                assert!(resolved.follow.is_empty());
                assert!(resolved.work_repositories.is_empty());
            }
            let sources = Sources::take(&mut value).unwrap();
            let mut lines = Vec::new();
            if matches!(
                case,
                "explicit missing parent" | "mixed sources missing parent"
            ) {
                let error = sources
                    .resolve(&mut value, &recorder, &mut lines)
                    .unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("followed midstream parent missing")
                );
                continue;
            }
            let resolved = sources
                .resolve(&mut value, &recorder, &mut lines)
                .unwrap()
                .unwrap();
            assert_eq!(
                value["follow"].as_array().unwrap().len(),
                usize::from(included),
                "{case}: {value}"
            );
            assert_eq!(
                value["workRepositories"].as_array().unwrap().len(),
                usize::from(included && !matches!(case, "explicit excluded")),
                "{case}"
            );
            assert_eq!(
                value["pr"]["noMerge"].as_array().unwrap().len(),
                usize::from(promoted),
                "{case}"
            );
            assert_eq!(resolved.len(), usize::from(promoted), "{case}");
            assert!(
                !lines
                    .iter()
                    .any(|line| line.starts_with("repositories: org ")
                        || line.starts_with("repositories: /")),
                "{lines:?}"
            );
            if promoted {
                assert_eq!(
                    value["promote"]["forks"],
                    if explicit {
                        json!({"TEAM/repo":{"upstream":"other/repo","base":"stable"}})
                    } else {
                        json!({"team/repo":{"upstream":"up/repo","base":"trunk"}})
                    },
                    "{case}"
                );
            }
            if !parent {
                assert!(
                    lines
                        .iter()
                        .any(|l| l.contains("refused team/repo: midstream parent missing"))
                );
            }
            assert_eq!(
                lines
                    .iter()
                    .filter(|l| l.starts_with("repositories: metadata"))
                    .count(),
                1
            );
            let recorded: Value =
                serde_json::from_str(&recorder.fixture(replay.now).unwrap()).unwrap();
            assert_eq!(
                recorded["requests"].as_array().unwrap().len(),
                1 + usize::from(
                    origin == "fork" && topics == json!(["bot-midstream"]) && !archived
                ),
                "{case}"
            );
        }
    }

    #[test]
    fn metadata_failures_refuse_configuration() {
        for (status, body) in [
            (403, json!({})),
            (200, json!({"full_name":"other/repo","fork":true})),
            (
                200,
                json!({"full_name":"team/repo","fork":true,"parent":{"full_name":"up/repo"}}),
            ),
        ] {
            let fixture = json!({"schema":"agent-board/pass-replay/v1","now":"2026-11-10T00:00:00Z", "requests":[
                {"request":{"get":format!("{API_ROOT}orgs/team/repos?type=public&sort=full_name&per_page=100&page=1")},
                 "answer":{"status":200,"body":[{"full_name":"team/repo","fork":true,"archived":false,"private":false,
                    "custom_properties":{"origin":"fork"},"topics":["bot-midstream"]}]}},
                {"request":{"get":format!("{API_ROOT}repos/team/repo")},"answer":{"status":status,"body":body}}
            ]});
            let replay = Replay::from_json(&fixture).unwrap();
            let mut value = json!({"midstream":{"owner":"team","topic":"bot-midstream"},"follow":[{"org":"team","forks":true}]});
            let sources = Sources::take(&mut value).unwrap();
            assert!(
                sources
                    .resolve(&mut value, &replay, &mut Vec::new())
                    .is_err()
            );
        }
    }

    #[test]
    fn explicit_private_metadata_is_validated_before_classification() {
        let repo = json!({"full_name":"team/repo","fork":true,"archived":false,"private":true,
            "custom_properties":{"origin":"fork"},"topics":["bot-midstream"],
            "parent":{"full_name":"up/repo","default_branch":"main"}});
        let mut cases = vec![("valid".to_owned(), repo.clone(), true)];
        for field in ["fork", "archived", "private", "custom_properties"] {
            let mut missing = repo.clone();
            missing.as_object_mut().unwrap().remove(field);
            cases.push((format!("missing {field}"), missing, false));
            for invalid in [Value::Null, json!("false"), json!([])] {
                let mut body = repo.clone();
                body[field] = invalid;
                cases.push((format!("invalid {field}: {}", body[field]), body, false));
            }
        }
        for invalid in [Value::Null, json!(true), json!([])] {
            let mut body = repo.clone();
            body["custom_properties"]["origin"] = invalid;
            cases.push(("invalid origin".to_owned(), body, false));
        }
        let mut missing_parent = repo.clone();
        missing_parent.as_object_mut().unwrap().remove("parent");
        cases.push(("missing parent".to_owned(), missing_parent, false));
        let mut null_parent = repo.clone();
        null_parent["parent"] = Value::Null;
        cases.push(("null parent".to_owned(), null_parent, false));
        for (case, body, valid) in cases {
            let fixture = json!({"schema":"agent-board/pass-replay/v1","now":"2026-11-10T00:00:00Z", "requests":[
                {"request":{"get":format!("{API_ROOT}orgs/team/repos?type=public&sort=full_name&per_page=100&page=1")},
                 "answer":{"status":200,"body":[]}},
                {"request":{"get":format!("{API_ROOT}repos/team/repo")},"answer":{"status":200,"body":body}}
            ]});
            let replay = Replay::from_json(&fixture).unwrap();
            let mut value = json!({"midstream":{"owner":"team","topic":"bot-midstream"},
                "follow":[{"org":"team","forks":true},"team/repo"]});
            let sources = Sources::take(&mut value).unwrap();
            let result = sources.resolve(&mut value, &replay, &mut Vec::new());
            assert_eq!(result.is_ok(), valid, "{case}: {result:?}");
            if valid {
                assert_eq!(result.unwrap(), Some(vec!["team/repo".to_owned()]));
                assert_eq!(value["pr"]["noMerge"], json!(["team/repo"]));
                assert_eq!(value["workRepositories"], json!(["team/repo"]));
            }
        }
    }

    #[test]
    fn public_discovery_keeps_issues_disabled_members_and_explicit_private_names() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../fixtures/pass/org-visibility.json"))
                .unwrap();
        let replay = Replay::from_json(&fixture).unwrap();
        for visibility in [None, Some("public")] {
            let mut org = json!({"org":"cgwalters-forge", "forks":false});
            if let Some(visibility) = visibility {
                org["visibility"] = json!(visibility);
            }
            let mut value = json!({
                "follow": [org], "workRepositories": [org],
                "sessions": {"repositories": [org]},
                "holds": {"proposals": {"repositories": [org]}},
            });
            let sources = Sources::take(&mut value).unwrap();
            sources
                .resolve(&mut value, &replay, &mut Vec::new())
                .unwrap();
            for path in [
                "/follow",
                "/workRepositories",
                "/sessions/repositories",
                "/holds/proposals/repositories",
            ] {
                assert_eq!(
                    value.pointer(path).unwrap(),
                    &json!(["cgwalters-forge/.github", "cgwalters-forge/agent-board"]),
                    "{path}"
                );
            }
            let mut value = json!({"follow": [org, "cgwalters-forge/private"]});
            let sources = Sources::take(&mut value).unwrap();
            sources
                .resolve(&mut value, &replay, &mut Vec::new())
                .unwrap();
            assert!(
                value["follow"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("cgwalters-forge/private"))
            );
        }
    }

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
                json!([{"full_name":"team/repo","fork":false,"archived":false}]),
                200,
                None,
                1,
            ),
            (
                json!([{"full_name":"team/repo","fork":false,"archived":false,"private":null}]),
                200,
                None,
                1,
            ),
            (
                json!([{"full_name":"team/repo","fork":false,"archived":false,"private":"false"}]),
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
                {"full_name":"team/old","fork":false,"archived":true,"private":false},
                {"full_name":"team/fork","fork":true,"archived":false,"private":false}
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
            json!({"org":"team"}),
            json!({"org":"team","forks":false,"extra":true}),
            json!({"org":"team","forks":false,"visibility":"private"}),
            json!({"org":"team","forks":false,"visibility":"all"}),
            json!({"org":"team","forks":false,"visibility":null}),
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
                body: json!([{"full_name":format!("cgwalters-forge/{name}"),"fork":false,"archived":false,"private":false}]),
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
                        "fork": false, "archived": false, "private": false
                    }))
                    .collect::<Vec<_>>()
            );
            // The header URL is deliberately not an authority for the next GET.
            first["answer"]["link"] = json!("<https://other.invalid/anything>; rel=\"next\"");
            let mut second = first.clone();
            second["request"]["get"] = json!(format!(
                "{API_ROOT}orgs/cgwalters-forge/repos?type=public&sort=full_name&per_page={PAGE_SIZE}&page=2"
            ));
            second["answer"]["link"] = Value::Null;
            second["answer"]["body"] = json!([{"full_name":"cgwalters-forge/last","fork":false,"archived":false,"private":false}]);
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
