//! Cheap discovery is only a hint; the standalone evaluator proves permission.
use super::*;

pub(crate) const QUERY: &str = r#"query PromoteAwaiting($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    pullRequests(first: 100, states: OPEN, orderBy: {field: CREATED_AT, direction: ASC}) {
      pageInfo { hasNextPage }
      nodes {
        number headRefOid isDraft author { login }
        reviews(last: 100) {
          pageInfo { hasPreviousPage }
          nodes { state author { login } commit { oid } }
        }
        comments(last: 100) {
          pageInfo { hasPreviousPage }
          nodes { body author { login } }
        }
      }
    }
  }
}"#;

/// One bounded query per fork, no per-PR gate reads for ordinary waiting PRs.
/// Truncation is an error, not evidence of no candidates.
pub(crate) fn candidates(client: &impl Transport, config: &Config, repo: &str) -> Result<Vec<u64>> {
    validate_repository(repo)?;
    let (owner, name) = repo.split_once('/').context("invalid fork")?;
    let data = client.graphql(QUERY, serde_json::json!({"owner":owner,"name":name}))?;
    let pulls = &data["repository"]["pullRequests"];
    ensure!(
        pulls["pageInfo"]["hasNextPage"] == false,
        "promotion discovery truncated for {repo}"
    );
    let nodes = pulls["nodes"]
        .as_array()
        .context("missing promotion discovery")?;
    ensure!(nodes.len() <= PAGE_SIZE, "oversized promotion discovery");
    let mut selected = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for pull in nodes {
        let number = pull["number"]
            .as_u64()
            .filter(|n| *n > 0)
            .context("invalid discovery number")?;
        ensure!(seen.insert(number), "duplicate discovery number");
        let author = pull["author"]["login"]
            .as_str()
            .context("missing discovery author")?;
        if !author.eq_ignore_ascii_case(&config.bot) {
            continue;
        }
        let head = pull["headRefOid"]
            .as_str()
            .filter(|h| oid(h))
            .context("invalid discovery head")?;
        let draft = pull["isDraft"]
            .as_bool()
            .context("missing discovery draft flag")?;
        let mut rows = Vec::new();
        for key in ["reviews", "comments"] {
            ensure!(
                pull[key]["pageInfo"]["hasPreviousPage"] == false,
                "promotion {repo}#{number} {key} discovery truncated"
            );
            let nodes = pull[key]["nodes"]
                .as_array()
                .context("missing discovery evidence")?;
            ensure!(nodes.len() <= PAGE_SIZE, "oversized discovery evidence");
            rows.push(nodes);
        }
        let operator = |v: &&Value| {
            v["author"]["login"]
                .as_str()
                .is_some_and(|a| a.eq_ignore_ascii_case(&config.operator))
        };
        let approval = config.promote.as_ref().is_some_and(|p| p.on_approval)
            && draft
            && rows[0]
                .iter()
                .filter(operator)
                .rev()
                .find(|r| r["state"] != "COMMENTED")
                .is_some_and(|r| r["state"] == "APPROVED" && r["commit"]["oid"] == head);
        let command = rows[1].iter().filter(operator).any(|c| {
            c["body"]
                .as_str()
                .is_some_and(|b| command_line(b).is_ok_and(|c| c.is_some()))
        });
        if approval || command {
            selected.insert(number);
        }
    }
    Ok(selected.into_iter().collect())
}

pub(crate) struct Awaiting<'a> {
    config: &'a Config,
    policies: Policies,
}

impl<'a> Awaiting<'a> {
    pub(crate) fn new(config: &'a Config) -> Self {
        Self {
            config,
            policies: BTreeMap::new(),
        }
    }

    pub(crate) fn reason(
        &mut self,
        client: &(impl Rest + Transport),
        repo: &str,
        number: u64,
        now: OffsetDateTime,
    ) -> Result<String> {
        self.reason_impl(client, repo, number, now).map_err(|e| {
            if e.is::<Held>() || e.is::<NotSelected>() {
                e
            } else {
                read_failure(e)
            }
        })
    }

    fn reason_impl(
        &mut self,
        client: &(impl Rest + Transport),
        repo: &str,
        number: u64,
        now: OffsetDateTime,
    ) -> Result<String> {
        let target = self
            .config
            .promote
            .as_ref()
            .context("promote configuration")?
            .forks
            .get(repo)
            .context("unknown promotion fork")?;
        let pull = get(client, &format!("repos/{repo}/pulls/{number}"))?;
        ensure!(pull["number"] == number, "promotion pull identity mismatch");
        for path in [
            "/state",
            "/body",
            "/title",
            "/user/login",
            "/head/sha",
            "/head/ref",
            "/head/repo/full_name",
            "/head/repo/owner/login",
        ] {
            ensure!(
                pull.pointer(path).is_some_and(Value::is_string),
                "missing promotion pull field {path}"
            );
        }
        ensure!(pull["draft"].is_boolean(), "missing promotion draft flag");
        let metadata = get(client, &format!("repos/{repo}"))?;
        repository(&metadata, repo)?;
        let command = evaluate(
            client,
            self.config,
            (repo, number),
            &pull,
            (
                observed_network(&metadata, &target.upstream)?,
                &target.upstream,
            ),
            now,
        )?;
        let (gate, permission) = assess(
            client,
            self.config,
            repo,
            &pull,
            target,
            &command,
            &mut self.policies,
        )?;
        permission.map_err(GateFailure::into_error)?;
        match gate {
            Gates::Promoted(line) => Ok(line),
            Gates::Ready { signoffs, .. } => {
                hold!(signoffs == 0, "DCO sign-off rewrite required");
                hold!(
                    !pull["head"]["repo"]["owner"]["login"]
                        .as_str()
                        .is_some_and(|o| o.eq_ignore_ascii_case(&self.config.bot)),
                    "bot-fork staging unverified"
                );
                Ok("ready to publish".into())
            }
        }
    }
}
