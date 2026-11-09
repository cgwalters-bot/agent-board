//! `guidance` in the configuration (agent-board#788): where each
//! repository's own instructions are, which the triage briefs the pass writes
//! point the run at rather than restating. A repository's guidance is its
//! own file; a midstream fork's lives elsewhere, in agenthome, pinned to a
//! commit. The pass names the file; the run reads it.
use super::*;

/// The most repositories `guidance` may name.
const GUIDANCE_LIMIT: usize = POLL_REPOSITORY_LIMIT;

/// The longest path of a guidance file.
const PATH_BYTES: usize = 200;

/// `guidance`, keyed by the repository whose issues' briefs name it.
pub type Guidance = BTreeMap<String, Source>;

/// One repository's guidance file. Another repository's must be pinned to
/// a full commit: unpinned, whoever writes there would change every brief
/// that names it, and a brief would not say which text it was built from.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// None: the repository itself, as the run checks it out.
    pub repository: Option<String>,
    pub path: String,
    /// A full commit of `repository`; none: the run's checkout.
    #[serde(rename = "ref")]
    pub revision: Option<String>,
}

/// Whether `path` is a plain relative path inside a repository: no `..`
/// leaving it, no absolute path or URL, nothing a brief would render as
/// markup.
fn plain_path(path: &str) -> bool {
    path.len() <= PATH_BYTES
        && path.split('/').all(|part| {
            !matches!(part, "" | "." | "..")
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
}

fn full_commit(revision: &str) -> bool {
    revision.len() == 40
        && revision
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub(super) fn check(guidance: &Guidance) -> Result<()> {
    ensure!(
        guidance.len() <= GUIDANCE_LIMIT,
        "guidance names more than {GUIDANCE_LIMIT} repositories"
    );
    let mut seen = BTreeSet::new();
    for (repo, source) in guidance {
        validate_repository(repo).context("guidance")?;
        ensure!(
            seen.insert(repo.to_ascii_lowercase()),
            "guidance names {repo} twice"
        );
        ensure!(
            plain_path(&source.path),
            "guidance {repo} path must be a relative path of at most {PATH_BYTES} bytes of letters, digits, '.', '_', '-' and '/', without empty, '.' or '..' parts"
        );
        if let Some(revision) = &source.revision {
            ensure!(
                full_commit(revision),
                "guidance {repo} ref must be a full lowercase commit"
            );
        }
        if let Some(at) = &source.repository {
            validate_repository(at).with_context(|| format!("guidance {repo} repository"))?;
            ensure!(
                at.eq_ignore_ascii_case(repo) || source.revision.is_some(),
                "guidance {repo} reads {at}, so it needs a ref"
            );
        }
    }
    Ok(())
}

/// The paragraph a brief for an issue of `repo` ends with, naming its
/// guidance; none where `guidance` has none for it.
pub(super) fn paragraph(guidance: &Guidance, repo: &str) -> Option<String> {
    let (_, source) = guidance
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(repo))?;
    let at = source.repository.as_deref().unwrap_or(repo);
    let file = match &source.revision {
        Some(revision) => format!(
            "`{path}` in {at} at {revision} (https://{DEFAULT_HOST}/{at}/blob/{revision}/{path})",
            path = source.path
        ),
        None => format!("`{}` in {at}", source.path),
    };
    Some(format!(
        "Repository guidance: read {file} before acting, as {repo}'s own instructions for how to build, test and review work there; if you cannot read it, say so in your answer. Follow it where this brief leaves a choice open, and name the same file in any job plan brief you write for {repo}."
    ))
}
