//! Stage 4b, promotion (agent-board#308, docs/pass.md#order-and-effects,
//! docs/architecture.md#promote): a fork pull request the operator approved
//! on its current head is opened in the upstream project its fork tracks.
//!
//! Built so far: the configuration and the stage's one line. Nothing is
//! evaluated, read or written yet; the read-only evaluation comes next, then
//! each write behind `--apply` and `--max-writes`.
use super::*;

/// `promote` in the configuration ([`Config::promote`]): which of our forks
/// send approved pull requests to which upstream, and as whom. Configuration,
/// never a pull request's body, which anyone can edit, says where one goes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Promote {
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
                !config.follow.iter().any(|r| r.eq_ignore_ascii_case(fork)),
                "promote fork {fork} is also in follow; a fork's pull requests are promoted, never merged"
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

/// The stage's one line: off, or how many forks are configured.
pub(super) fn line(promote: Option<&Promote>) -> String {
    match promote.map(|p| p.forks.len()).unwrap_or(0) {
        0 => "promote: off".into(),
        forks => format!("promote: {forks} fork(s) configured; evaluation is not built"),
    }
}
