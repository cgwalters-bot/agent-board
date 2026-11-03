//! Ordinary review intake is deliberately independent of merge/fix authority.
use super::*;

const WORKFLOW: &str = "dispatch.yml";
const POLICY_MAX: usize = 64 * 1024;
const AUTO_POLICY: &str = "workflow/review.toml";
const COMMAND_POLICY: &str = "workflow/review-command.toml";
// Ordinary intake must leave the shared request budget for merges and fixes.
pub(super) const INTAKE_PULLS: usize = 10;
pub(super) const INTAKE_ACTORS: usize = 10;

#[derive(Default)]
pub(super) struct Intake {
    policies: BTreeMap<String, Option<Policy>>,
    permissions: BTreeMap<String, String>,
    pub(super) extra_pulls: usize,
    pub(super) pulls: usize,
}

type IntakeStep = (String, bool, Option<Wait>, Option<Stepped>);

pub(super) fn ordinary_reviews(config: &Config, repo: &str) -> bool {
    config
        .policy
        .review_dispatch_repositories
        .iter()
        .any(|r| r.eq_ignore_ascii_case(repo))
}

pub(super) fn review_workflow<'a>(config: &'a Config, repo: &str) -> Option<&'a str> {
    if ordinary_reviews(config, repo) {
        Some(WORKFLOW)
    } else {
        config.review_workflow.as_deref()
    }
}

// The request id is part of the existing durable head reservation, not a second
// queue. An automatic start also respects a command's reservation for its head.
pub(super) fn review_key(pull: &Value) -> String {
    let head = text(pull, "/headRefOid");
    match pull["reviewRequest"]["id"].as_u64() {
        Some(id) => format!("{head}/{id}"),
        None => head.to_owned(),
    }
}

pub(super) fn review_reserves(marker: &Value, pull: &Value) -> bool {
    let reserved = text(marker, "/review_head");
    reserved == review_key(pull)
        || (pull["reviewRequest"].is_null()
            && reserved
                .strip_prefix(text(pull, "/headRefOid"))
                .is_some_and(|s| s.starts_with('/')))
}

pub(super) fn after_request(pull: &Value, run: &Value) -> bool {
    pull["reviewRequest"].is_null()
        || timestamp(pull["reviewRequest"]["reserved_at"].as_str())
            .zip(timestamp(run["created_at"].as_str()))
            .is_some_and(|(request, run)| run >= request)
}

/// The security-relevant subset of the base-owned policy. Unknown output
/// settings are left to the caller; unsupported admission patterns fail closed.
#[derive(Clone)]
struct Policy {
    root: toml::Value,
    trigger: Trigger,
}

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Trigger {
    events: Vec<String>,
    roles: Vec<String>,
    bots: Vec<String>,
    forks: bool,
    #[serde(default)]
    commands: Vec<String>,
}

impl Policy {
    fn has(&self, path: &[&str], value: &str) -> bool {
        let mut node = &self.root;
        for key in path {
            let Some(next) = node.get(key) else {
                return false;
            };
            node = next;
        }
        node.as_array().is_some_and(|a| {
            a.iter().any(|v| {
                v.as_str().is_some_and(|s| {
                    if path == ["bases"] {
                        s == value
                    } else {
                        s.eq_ignore_ascii_case(value)
                    }
                })
            })
        })
    }

    fn trigger(&self, key: &str, value: &str) -> bool {
        let values = match key {
            "events" => &self.trigger.events,
            "roles" => &self.trigger.roles,
            "bots" => &self.trigger.bots,
            "commands" => &self.trigger.commands,
            _ => return false,
        };
        values.iter().any(|v| v.eq_ignore_ascii_case(value))
    }

    fn admits(&self, repo: &str, pull: &Value, command: bool) -> bool {
        self.has(&["hosts"], "github.com")
            && self.has(&["repos"], repo)
            && self.has(&["bases"], text(pull, "/baseRefName"))
            && self.trigger(
                "events",
                if command {
                    "issue_comment"
                } else {
                    REVIEW_EVENT
                },
            )
            && (pull["isCrossRepository"] == false || self.trigger.forks)
    }
}

impl<R: Rest, G: Transport> Pass<'_, R, G> {
    /// At most one REST page, and never an incomplete history. Bot-owned PRs
    /// retain their existing complete reads for merge/fix claim recovery.
    pub(super) fn review_comments(
        &mut self,
        repo: &str,
        number: u64,
        pull: &Value,
    ) -> Result<Option<Vec<Value>>> {
        if pull.pointer("/comments/pageInfo/hasPreviousPage") == Some(&Value::Bool(false)) {
            return self.pull_comments(repo, number, pull).map(Some);
        }
        let (body, next) = self.get_url(
            &format!("{API_ROOT}repos/{repo}/issues/{number}/comments?per_page=100"),
            Value::clone,
            self.apply.is_some(),
        )?;
        if next.is_some() {
            return Ok(None);
        }
        Ok(Some(
            body.as_array()
                .context("review comments are not an array")?
                .clone(),
        ))
    }

    fn review_policy(&mut self, repo: &str, pull: &Value, command: bool) -> Option<Policy> {
        let key = format!("{repo}/{command}").to_ascii_lowercase();
        if let Some(policy) = self.review_intake.policies.get(&key) {
            return policy.clone();
        }
        // Missing or unreadable policy disables only this intake route. Cache
        // failures as well: untrusted comments must not cause repeated reads.
        let policy = self.read_review_policy(repo, pull, command).ok();
        self.review_intake.policies.insert(key, policy.clone());
        policy
    }

    fn read_review_policy(&mut self, repo: &str, pull: &Value, command: bool) -> Result<Policy> {
        let default = text(pull, "/baseRepository/defaultBranchRef/name");
        ensure!(!default.is_empty(), "review base default branch not read");
        let encoded: String = url::form_urlencoded::byte_serialize(default.as_bytes()).collect();
        let path = if command { COMMAND_POLICY } else { AUTO_POLICY };
        let file = self.get(&format!("repos/{repo}/contents/{path}?ref={encoded}"))?;
        ensure!(
            file["type"] == "file"
                && file["encoding"] == "base64"
                && file["size"]
                    .as_u64()
                    .is_some_and(|n| n <= POLICY_MAX as u64),
            "invalid base-owned review policy {path}"
        );
        let bytes = crate::supervise::health::base64_decode(
            file["content"].as_str().context("review policy content")?,
        )?;
        ensure!(
            bytes.len() <= POLICY_MAX,
            "review policy exceeds {POLICY_MAX} bytes"
        );
        let policy: toml::Value = toml::from_str(std::str::from_utf8(&bytes)?)
            .context("base-owned review policy TOML")?;
        let trigger: Trigger = policy
            .get("github")
            .and_then(|g| g.get("trigger"))
            .context("review policy has no github.trigger")?
            .clone()
            .try_into()
            .context("unsupported review admission policy")?;
        Ok(Policy {
            root: policy,
            trigger,
        })
    }

    fn review_command(
        &mut self,
        repo: &str,
        pull: &Value,
        comments: &[Value],
    ) -> Result<Option<Value>> {
        let mut commands: Vec<_> = comments
            .iter()
            .filter(|c| {
                text(c, "/body").trim() == "/review"
                    && c["user"]["type"] == "User"
                    && c["id"].as_u64().is_some()
                    && timestamp(c["created_at"].as_str()).is_some()
            })
            .collect();
        if commands.is_empty() {
            return Ok(None);
        }
        let Some(policy) = self.review_policy(repo, pull, true) else {
            return Ok(None);
        };
        if !policy.admits(repo, pull, true) || !policy.trigger("commands", "/review") {
            return Ok(None);
        }
        commands.sort_by_key(|c| c["id"].as_u64());
        let consumed = |c: &Value| {
            let id = c["id"].as_u64();
            let matches = |key: &str| {
                key.split_once('/')
                    .is_some_and(|(_, suffix)| suffix.parse::<u64>().ok() == id)
            };
            comments
                .iter()
                .filter(|m| text(m, "/user/login") == self.config.bot)
                .any(|m| {
                    parse_marker(MARKER, text(m, "/body"))
                        .is_some_and(|m| matches(text(&m, "/review_head")))
                        || parse_marker(CLAIM, text(m, "/body")).is_some_and(|m| {
                            text(&m, "/from")
                                .strip_prefix("review:")
                                .is_some_and(matches)
                        })
                })
        };
        // Reauthorize pending work, not every historical request. Keep only the
        // newest consumed request *for this head* to recover its run. A command
        // is consumed once across all heads, including a claim whose response
        // was lost; pushing a head cannot renew the reviewer's authorization.
        let (consumed, pending): (Vec<_>, Vec<_>) = commands.into_iter().partition(|c| consumed(c));
        let recovery = consumed.into_iter().rev().find(|c| {
            let mut requested = pull.clone();
            requested["reviewRequest"] = (*c).clone();
            comments
                .iter()
                .filter(|m| text(m, "/user/login") == self.config.bot)
                .any(|m| {
                    parse_marker(MARKER, text(m, "/body"))
                        .is_some_and(|m| review_reserves(&m, &requested))
                        || earliest_claim(
                            std::slice::from_ref(m),
                            &self.config.bot,
                            &format!("{REVIEW}:{}", review_key(&requested)),
                        )
                        .is_some()
                })
        });
        let mut selected = None;
        for comment in pending.into_iter().chain(recovery) {
            let login = text(comment, "/user/login");
            if !is_github_login(login) {
                continue;
            }
            let key = format!("{repo}/{login}").to_ascii_lowercase();
            // Association is not permission. Cache current base permissions per
            // actor for this pass, so repeated untrusted commands cannot spend
            // one request each (even when conditional HTTP caching is in use).
            if !self.review_intake.permissions.contains_key(&key) {
                if self.review_intake.permissions.len() >= INTAKE_ACTORS {
                    continue;
                }
                let permission =
                    self.get(&format!("repos/{repo}/collaborators/{login}/permission"));
                self.review_intake.permissions.insert(
                    key.clone(),
                    permission
                        .ok()
                        .map(|p| text(&p, "/permission").to_owned())
                        .unwrap_or_default(),
                );
            }
            let role = self
                .review_intake
                .permissions
                .get(&key)
                .map(String::as_str)
                .unwrap_or("");
            if ["admin", "maintain", "write"].contains(&role) && policy.trigger("roles", role) {
                selected = Some(comment.clone());
                break;
            }
        }
        if let Some(command) = &mut selected {
            let mut requested = pull.clone();
            requested["reviewRequest"] = command.clone();
            let reservation = comments
                .iter()
                .filter(|m| text(m, "/user/login") == self.config.bot)
                .filter_map(|m| parse_marker(MARKER, text(m, "/body")))
                .find(|m| review_reserves(m, &requested));
            if let Some(reservation) = reservation {
                command["reserved_at"] = reservation["review_at"].clone();
            }
        }
        Ok(selected)
    }

    /// Returns a review-only step when intake needs work. Otherwise the original
    /// merge/fix decision proceeds unchanged, even for an approved ordinary run.
    pub(super) fn ordinary_review(
        &mut self,
        repo: &str,
        pull: &Value,
        comments: &[Value],
        marker: &Marker,
    ) -> Result<Option<IntakeStep>> {
        if pull["isDraft"] != false
            || pull["baseRepository"]["isPrivate"] != false
            || is_held(pull)
            || labelled(pull)
        {
            return Ok(None);
        }
        let number = pull["number"].as_u64().context("review pull number")?;
        if pull.pointer("/labels/pageInfo/hasNextPage") != Some(&Value::Bool(false))
            || pull.pointer("/reviews/pageInfo/hasPreviousPage") != Some(&Value::Bool(false))
        {
            return Ok(Some((
                format!("{repo}#{number} wait: review intake labels or reviews not read whole"),
                false,
                None,
                None,
            )));
        }
        let head = text(pull, "/headRefOid");
        ensure!(
            head.len() == 40 && head.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid review head"
        );
        let mut requested = pull.clone();
        let command = self.review_command(repo, pull, comments)?;
        if let Some(command) = command {
            requested["reviewRequest"] = command;
        } else {
            let Some(policy) = self.review_policy(repo, pull, false) else {
                return Ok(None);
            };
            if !policy.admits(repo, pull, false) {
                return Ok(None);
            }
            let raw = self.get(&format!("repos/{repo}/pulls/{number}"))?;
            if raw["state"] != "open" || raw["draft"] != false || text(&raw, "/head/sha") != head {
                return Ok(Some((
                    format!("{repo}#{number} wait: review intake changed"),
                    false,
                    None,
                    None,
                )));
            }
            let associated =
                ["OWNER", "MEMBER", "COLLABORATOR"].contains(&text(&raw, "/author_association"));
            if !associated {
                if raw["user"]["type"] != "Bot"
                    || !policy.trigger("bots", text(&raw, "/user/login"))
                {
                    return Ok(None);
                }
                let ids = self.get(&format!("repos/{repo}/actions/variables/REVIEW_BOT_IDS"))?;
                let ids: Vec<u64> =
                    serde_json::from_str(ids["value"].as_str().context("REVIEW_BOT_IDS value")?)?;
                if !raw["user"]["id"]
                    .as_u64()
                    .is_some_and(|id| ids.contains(&id))
                {
                    return Ok(None);
                }
            }
            if !policy.trigger("roles", "all") {
                return Ok(None);
            }
        }
        if let Some((_, review)) = job_verdict(pull)
            && let Some((base, id)) = cited_run(text(review, "/body"))
            && base == repo
        {
            let run = self.get(&format!("repos/{repo}/actions/runs/{id}"))?;
            if (review_run_matches(pull, review, Some(&run), self.config)
                || review_run_for_workflow(
                    pull,
                    review,
                    Some(&run),
                    self.config,
                    review_workflow(self.config, repo),
                ))
                && after_request(&requested, &run)
            {
                return Ok(None);
            }
        }
        let Some((decision, run)) = self.review_dispatch(repo, &requested, comments)? else {
            return Ok(None);
        };
        // Intake's recovery hold must not swallow the bot's normal conflict,
        // fix or needs-human path. No authority is added for other authors.
        if !matches!(decision.action, REVIEW | REVIEW_RERUN)
            && (text(pull, "/author/login") == self.config.bot
                || self
                    .config
                    .policy
                    .merge_authors
                    .iter()
                    .any(|a| a == text(pull, "/author/login")))
        {
            return Ok(None);
        }
        let line = format!(
            "{} {repo}#{number} {}: {}",
            decision.action,
            short(head),
            decision.reason
        );
        if matches!(decision.action, REVIEW | REVIEW_RERUN) {
            let rank = nodes(pull, "/closingIssuesReferences/nodes")
                .iter()
                .filter_map(|i| {
                    let key = format!(
                        "{}#{}",
                        text(i, "/repository/nameWithOwner"),
                        i["number"].as_u64()?
                    );
                    self.config
                        .policy
                        .review_ranks
                        .get(&key.to_ascii_lowercase())
                        .copied()
                })
                .min()
                .unwrap_or(usize::MAX);
            self.deferring = Some(Deferred {
                rank,
                repo: repo.to_owned(),
                number,
                pull: requested,
                marker: marker.clone(),
                comments: comments.to_vec(),
                action: decision.action,
                run,
                line: line.clone(),
                at: (0, None),
            });
        }
        // Recovery holds are reported, never converted into merge authority or
        // new fix/label writes on someone else's pull request.
        Ok(Some((line, false, None, None)))
    }
}
