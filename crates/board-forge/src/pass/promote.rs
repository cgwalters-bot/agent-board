//! Shared promotion configuration and read-only pass visibility.
use super::*;

/// `promote` in the configuration ([`Config::promote`]): which of our forks
/// send command-authorized pull requests to which upstream, and as whom. Configuration,
/// never a pull request's body, which anyone can edit, says where one goes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Promote {
    /// Permit standalone publication with --apply. Off by default so existing
    /// preview callers cannot unexpectedly publish to an upstream repository.
    #[serde(default)]
    pub publish: bool,

    /// Accept drafts as candidates, as a midstream's pull requests stay drafts,
    /// and on a draft the operator's approving review of the current head,
    /// submitted after its push, as the trigger. Off by default; `/promote`
    /// still triggers.
    #[serde(default)]
    pub on_approval: bool,

    /// Each fork repository to its upstream and base branch; empty is off.
    #[serde(default)]
    pub forks: BTreeMap<String, Fork>,
    /// The operator's `Signed-off-by` and committer identity; required with
    /// `forks`, as a person's identity has no default.
    pub signoff: Option<Signoff>,
    /// Where contribution-policy records are read, from its default branch
    /// only; required with `forks`.
    pub policy_repository: Option<String>,
    /// The directory of the records in it; None: its top level.
    pub policy_path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fork {
    pub upstream: String,
    pub base: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signoff {
    pub name: String,
    pub email: String,
}

impl Promote {
    pub(super) fn check(&self, config: &Config) -> Result<()> {
        let mut seen = BTreeSet::new();
        for (fork, to) in &self.forks {
            validate_repository(fork).context("promote forks")?;
            // GitHub names are case-insensitive: one fork, one destination.
            ensure!(
                seen.insert(fork.to_ascii_lowercase()),
                "promote forks names {fork} twice"
            );
            validate_repository(&to.upstream)
                .with_context(|| format!("promote forks {fork} upstream"))?;
            ensure!(
                !fork.eq_ignore_ascii_case(&to.upstream),
                "promote fork {fork} is its own upstream"
            );
            ensure!(
                !to.base.trim().is_empty(),
                "promote forks {fork} base is empty"
            );
            // A midstream's main moves only by sync from its upstream: stage 2
            // must not merge or rebase its pull requests.
            ensure!(
                !config.follow.iter().any(|r| r.eq_ignore_ascii_case(fork))
                    || config.midstream_owns(fork),
                "promote fork {fork} is also in follow without midstream ownership; a fork's pull requests are promoted, never merged"
            );
        }
        if let Some(signoff) = &self.signoff {
            ensure!(
                !signoff.name.trim().is_empty() && !signoff.email.trim().is_empty(),
                "promote signoff name and email must not be empty"
            );
        }
        if let Some(policy) = &self.policy_repository {
            validate_repository(policy).context("promote policyRepository")?;
        }
        if !self.forks.is_empty() {
            ensure!(
                self.signoff.is_some(),
                "promote signoff is required with forks: the operator's Signed-off-by"
            );
            ensure!(
                self.policy_repository.is_some(),
                "promote policyRepository is required with forks: where contribution-policy records are read"
            );
        }
        Ok(())
    }
}

/// Refuses mutations even when the surrounding pass is applying. Every read
/// still uses its Budget; the reserve prevents diagnostics starving later work.
struct Reads<'a, 'b, C> {
    client: &'a Budget<'b, C>,
    exhausted: Cell<bool>,
    failed: RefCell<Option<String>>,
}

impl<C> Reads<'_, '_, C> {
    fn admit(&self) -> Result<()> {
        let b = self.client.bounds;
        if b.left.get() <= WRITE_RESERVE + b.reserve.get().max(b.kept.get())
            || Instant::now() > b.admit_by
        {
            self.exhausted.set(true);
            bail!("promotion read budget spent");
        }
        Ok(())
    }

    fn observe<T>(&self, result: Result<T>) -> Result<T> {
        if let Err(e) = &result
            && !e.is::<crate::github::NotFound>()
        {
            *self.failed.borrow_mut() = Some(format!("{e:#}"));
        }
        result
    }
}

impl<C: Rest> Rest for Reads<'_, '_, C> {
    fn get(&self, url: &str, validator: Option<(&str, &str)>) -> Result<RestResponse> {
        self.admit()?;
        let response = self.observe(self.client.get(url, validator))?;
        // 404 can mean a missing policy record or optional directory. Those
        // are gate facts, not transport failures.
        if response.status != 200 && response.status != 404 {
            *self.failed.borrow_mut() = Some(format!("GET {url}: HTTP {}", response.status));
        }
        Ok(response)
    }

    fn send(&self, _: &str, _: &str, _: &Value) -> Result<(u16, Value)> {
        bail!("promotion pass view is read-only")
    }
}

impl<C: Transport> Transport for Reads<'_, '_, C> {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        ensure!(!is_mutation(query), "promotion pass view is read-only");
        self.admit()?;
        self.observe(self.client.graphql(query, variables))
    }
}

impl<C: Rest + Transport> Scheduler<'_, C> {
    pub(super) fn promote(&mut self) {
        let (lines, failures) = view(&self.client, self.config, self.now);
        self.report.failures += failures;
        for line in lines {
            self.line(line);
        }
    }
}

pub(super) fn view<C: Rest + Transport>(
    client: &Budget<'_, C>,
    config: &Config,
    now: OffsetDateTime,
) -> (Vec<String>, usize) {
    let mut lines = Vec::new();
    let mut failures = 0;
    if !crate::promote::enabled(config) {
        return (vec!["promote: off".into()], 0);
    }
    let reads = Reads {
        client,
        exhausted: Cell::new(false),
        failed: RefCell::new(None),
    };
    let forks = &config.promote.as_ref().map(|p| &p.forks);
    let mut candidates = Vec::new();
    if let Some(forks) = forks {
        for repo in forks.keys() {
            match crate::promote::awaiting::candidates(&reads, config, repo) {
                Ok(numbers) => candidates.extend(numbers.into_iter().map(|n| (repo.as_str(), n))),
                Err(e) => {
                    if reads.exhausted.get() {
                        lines.push(format!(
                            "promote: discovery incomplete (budget); fork {repo} not evaluated"
                        ));
                        break;
                    }
                    failures += 1;
                    lines.push(format!("promote error: {repo}: {e:#}"));
                    reads.failed.borrow_mut().take();
                }
            }
        }
    }
    let mut evaluator = crate::promote::awaiting::Awaiting::new(config);
    for (i, (repo, number)) in candidates.iter().enumerate() {
        let result = evaluator.reason(&reads, repo, *number, now);
        if reads.exhausted.get() {
            lines.push(format!(
                "promote: {} candidate(s) not evaluated (budget)",
                candidates.len() - i
            ));
            break;
        }
        if result
            .as_ref()
            .err()
            .is_some_and(|e| e.is::<crate::promote::NotSelected>())
            && reads.failed.borrow().is_none()
        {
            continue;
        }
        let failed = reads.failed.borrow_mut().take().or_else(|| {
            result
                .as_ref()
                .err()
                .filter(|e| e.is::<crate::promote::ReadFailure>())
                .map(|e| format!("{e:#}"))
        });
        if let Some(error) = failed {
            failures += 1;
            lines.push(format!("promote error: {repo}#{number}: {error}"));
        } else {
            let reason = result.unwrap_or_else(|e| format!("{e:#}"));
            lines.push(format!("promote awaiting {repo}#{number}: {reason}"));
        }
    }
    (lines, failures)
}
