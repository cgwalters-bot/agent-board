//! Informational, head-bound draft classification; never merge/fix provenance.
use super::*;
use crate::plan::fenced_json;
use serde::{Deserialize, Serialize};

pub const RESULT_SCHEMA: &str = "agent-board/draft-review/v1";
pub const RESULT_FENCE: &str = "agent-board-draft-review";
pub const MAX_QUICK_FILES: usize = 5;
pub const MAX_QUICK_LINES: u64 = 150;
pub const MAX_RESULT_BYTES: usize = 8 * 1024;
pub const MAX_REASON_BYTES: usize = 1024;
/// Base-owned dispatch must reserve this job name for the review result writer.
pub const RESULT_JOB: &str = "agent-board-review-result-v1";
pub const MAX_REVIEW_JOBS: usize = 100;
pub(super) const REVIEW_TASK: &str = "Review the pinned draft head using the base-owned review task and bounds. Never mark ready, promote, merge, enqueue or fix it. Include exactly one agent-board-draft-review JSON fence in the submitted review using agent-board/draft-review/v1 (docs/poll.md): repository, pr, reviewed_head, title, description, semantic_diff, description_assessment, change_category, risk, evidence, reason. Assess semantics and description, not just paths/size; missing or uncertain evidence is deep.";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bucket {
    Quick,
    Deep,
}

impl Bucket {
    fn label(&self) -> &'static str {
        match self {
            Self::Quick => "review:quick",
            Self::Deep => "review:deep",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_size_and_unknown_path_vetoes_are_conservative() {
        for repo in ["bootc", "composefs-rs", "bcvk", "rpm-ostree"] {
            let repo = format!("cgwalters-forge/{repo}");
            for path in ["README.md", "docs/usage.md", "Cargo.toml", "Cargo.lock"] {
                assert!(safe_path(&repo, path), "{repo}: {path}");
            }
            for risk in [
                "boot",
                "install",
                "gc",
                "garbage",
                "locking",
                "on-disk",
                "format",
                "security-policy",
                "selinux",
                "ima",
                "signature",
                "privilege",
            ] {
                for path in [format!("docs/{risk}.md"), format!("src/{risk}.rs")] {
                    assert!(!safe_path(&repo, &path), "{repo}: {path}");
                }
            }
            for path in [
                "src/lib.rs",
                "unknown.md",
                "docs/../README.md",
                "/README.md",
                "docs//ok.md",
                "docs/binary.png",
            ] {
                assert!(!safe_path(&repo, path), "{repo}: {path}");
            }
            let file = json!({"filename":"README.md", "status":"modified", "changes":10, "patch":"@@ diff"});
            for (count, changes, next, expected) in [
                (1, 150, false, true),
                (1, 151, false, false),
                (5, 30, false, true),
                (6, 1, false, false),
                (1, 1, true, false),
                (1, 0, false, false),
            ] {
                let mut f = file.clone();
                f["changes"] = changes.into();
                assert_eq!(
                    file_veto(
                        &repo,
                        &json!({"changed_files":count}),
                        &json!(vec![f; count]),
                        next
                    )
                    .is_ok(),
                    expected
                );
            }
            let mut renamed = file.clone();
            renamed["previous_filename"] = "src/install.rs".into();
            assert!(
                file_veto(&repo, &json!({"changed_files":1}), &json!([renamed]), false).is_err()
            );
            assert!(file_veto(&repo, &json!({"changed_files":2}), &json!([file]), false).is_err());
        }
        assert!(!safe_path("o/unknown", "README.md"));
        assert!(safe_path("o/bootc", "site/src/content/docs/usage.md"));
        assert!(!safe_path("o/bcvk", "site/src/content/docs/usage.md"));
    }
}

/// Optional list-level needs-you extension, keyed by each member's PR identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assessment {
    pub bucket: Bucket,
    pub assessed_head: String,
    pub reason: String,
    pub summary: String,
    pub ci: String,
}

impl Assessment {
    fn deep(head: &str, reason: impl Into<String>) -> Self {
        Self {
            bucket: Bucket::Deep,
            assessed_head: head.into(),
            reason: reason
                .into()
                .replace(['\n', '\r'], " ")
                .chars()
                .take(MAX_REASON_BYTES / 4)
                .collect(),
            summary: "Pending fresh verified semantic review".into(),
            ci: "unknown".into(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewResult {
    schema: String,
    repository: String,
    pr: u64,
    reviewed_head: String,
    title: String,
    description: String,
    semantic_diff: String,
    description_assessment: String,
    change_category: String,
    risk: String,
    evidence: String,
    reason: String,
}

pub(super) fn eligible(config: &Config, repo: &str, pull: &Value) -> bool {
    // Threat: PR prose or labels opt arbitrary authors into paid draft work.
    pull["isDraft"] == true
        && text(pull, "/author/login") == config.bot
        && pull["baseRepository"]["isPrivate"] == false
        && ordinary_reviews(config, repo)
        && config
            .policy
            .draft_review_repositories
            .iter()
            .any(|r| r.eq_ignore_ascii_case(repo))
}

fn safe_path(repo: &str, path: &str) -> bool {
    // Threat: a benign renamed destination conceals removal of risky code.
    // Both names must pass; unknown paths never gain semantic authority.
    if path.is_empty()
        || path.starts_with('/')
        || path.split('/').any(|p| p == ".." || p.is_empty())
    {
        return false;
    }
    let lower = path.to_ascii_lowercase();
    if [
        "boot",
        "install",
        "gc",
        "garbage",
        "lock",
        "disk",
        "format",
        "security",
        "policy",
        "selinux",
        "ima",
        "signature",
        "privilege",
    ]
    .iter()
    .any(|risk| lower.contains(risk))
        && path != "Cargo.lock"
    {
        return false;
    }
    let Some((_, name)) = repo.rsplit_once('/') else {
        return false;
    };
    let docs = match name {
        "bootc" => &["docs/", "site/src/content/docs/"][..],
        "composefs-rs" => &["docs/"][..],
        "bcvk" => &["docs/"][..],
        "rpm-ostree" => &["docs/", "man/"][..],
        _ => return false,
    };
    matches!(
        path,
        "README.md" | "CONTRIBUTING.md" | "Cargo.toml" | "Cargo.lock"
    ) || docs
        .iter()
        .any(|prefix| path.starts_with(prefix) && path.ends_with(".md"))
        || matches!(
            (name, path),
            ("bootc", ".github/workflows/docs.yml")
                | ("composefs-rs", ".github/workflows/rust.yml")
                | ("bcvk", ".github/workflows/ci.yml")
                | ("rpm-ostree", ".github/workflows/docs.yml")
        )
}

fn file_veto(repo: &str, raw: &Value, files: &Value, next: bool) -> Result<()> {
    let files = files.as_array().context("incomplete file read")?;
    ensure!(
        !next && raw["changed_files"].as_u64() == Some(files.len() as u64),
        "incomplete file read"
    );
    ensure!(
        !files.is_empty() && files.len() <= MAX_QUICK_FILES,
        "scope exceeds MAX_QUICK_FILES ({MAX_QUICK_FILES})"
    );
    let mut lines = 0_u64;
    for file in files {
        ensure!(
            safe_path(repo, text(file, "/filename")),
            "risky or unknown path: {}",
            text(file, "/filename")
        );
        if let Some(previous) = file["previous_filename"].as_str() {
            ensure!(
                safe_path(repo, previous),
                "risky or unknown previous path: {previous}"
            );
        }
        ensure!(
            ["added", "modified", "removed", "renamed"].contains(&text(file, "/status")),
            "unknown file status"
        );
        let changes = file["changes"].as_u64().context("unknown file size")?;
        ensure!(
            changes > 0 && file["patch"].as_str().is_some_and(|p| !p.is_empty()),
            "unknown or binary file diff"
        );
        lines = lines.checked_add(changes).context("file size overflow")?;
        ensure!(
            lines <= MAX_QUICK_LINES,
            "scope exceeds MAX_QUICK_LINES ({MAX_QUICK_LINES})"
        );
    }
    Ok(())
}

fn result(pull: &Value, repo: &str, review: &Value) -> Result<ReviewResult> {
    let body = text(review, "/body");
    ensure!(body.len() <= MAX_RESULT_BYTES, "review result too large");
    let value = fenced_json(body, RESULT_FENCE)?.context("missing structured review result")?;
    let r: ReviewResult =
        serde_json::from_value(value).context("invalid structured review result")?;
    ensure!(
        r.schema == RESULT_SCHEMA
            && r.repository == repo
            && Some(r.pr) == pull["number"].as_u64()
            && r.reviewed_head == text(pull, "/headRefOid"),
        "stale or mismatched review result"
    );
    ensure!(
        r.title == text(pull, "/title") && r.description == text(pull, "/body"),
        "description changed since review"
    );
    ensure!(
        r.semantic_diff == "pass" && r.description_assessment == "matches" && r.risk == "none",
        "semantic review failed, mismatched or unknown risk"
    );
    ensure!(
        [
            "docs",
            "dependencies",
            "ci",
            "mechanical",
            "narrow_fix_with_test"
        ]
        .contains(&r.change_category.as_str()),
        "unknown or non-mechanical change category"
    );
    for text in [&r.evidence, &r.reason] {
        ensure!(
            !text.trim().is_empty()
                && text.len() <= MAX_REASON_BYTES
                && !text.contains(['\n', '\r']),
            "missing or invalid evidence/reason"
        );
    }
    Ok(r)
}

impl<R: Rest, G: Transport> Pass<'_, R, G> {
    fn draft_assessment(
        &mut self,
        repo: &str,
        pull: &mut Value,
        comments: &[Value],
    ) -> Result<Assessment> {
        let (verdict, review) = job_verdict(pull).context("missing current-head review")?;
        ensure!(
            verdict == Review::Approved,
            "semantic review did not approve"
        );
        ensure!(
            pull.pointer("/reviews/pageInfo/hasPreviousPage") == Some(&Value::Bool(false)),
            "incomplete reviews"
        );
        let (base, id) =
            cited_run(text(review, "/body")).context("unverified review provenance")?;
        ensure!(base == repo, "foreign review provenance");
        let run = self.get(&format!("repos/{repo}/actions/runs/{id}"))?;
        if let Some(change) = nodes(pull, "/baseChange/nodes").last() {
            ensure!(
                timestamp(change["createdAt"].as_str())
                    .zip(timestamp(run["run_started_at"].as_str()))
                    .is_some_and(|(changed, started)| started > changed),
                "base changed since review"
            );
        }
        // Threat: copied run titles/markers, or an unrelated default-branch job,
        // masquerade as review evidence. Require a real configured workflow run,
        // its submission interval AND the durable head reservation we dispatched.
        ensure!(
            review_run_for_workflow(
                pull,
                review,
                Some(&run),
                self.config,
                review_workflow(self.config, repo)
            ) && run["conclusion"] == "success",
            "unverified review provenance"
        );
        let (jobs, next) = self.get_url(&format!("{API_ROOT}repos/{repo}/actions/runs/{id}/attempts/{}/jobs?per_page={MAX_REVIEW_JOBS}", run["run_attempt"].as_u64().context("unknown review run attempt")?), Value::clone, true)?;
        let jobs_read = jobs["jobs"].as_array().context("review jobs unread")?;
        ensure!(
            next.is_none()
                && jobs["total_count"].as_u64() == Some(jobs_read.len() as u64)
                && jobs_read.len() <= MAX_REVIEW_JOBS,
            "incomplete review job provenance"
        );
        let writers: Vec<_> = jobs_read
            .iter()
            .filter(|j| j["name"] == RESULT_JOB)
            .collect();
        ensure!(
            writers.len() == 1
                && writers[0]["run_id"].as_u64() == Some(id)
                && writers[0]["status"] == "completed"
                && writers[0]["conclusion"] == "success"
                && timestamp(writers[0]["started_at"].as_str())
                    .zip(timestamp(writers[0]["completed_at"].as_str()))
                    .zip(timestamp(review["submittedAt"].as_str()))
                    .is_some_and(|((from, to), at)| from <= at && at <= to),
            "unverified configured review result job"
        );
        let reserved = comments
            .iter()
            .filter(|c| text(c, "/user/login") == self.config.bot)
            .filter_map(|c| parse_marker(MARKER, text(c, "/body")))
            .any(|m| {
                review_reserves(&m, pull)
                    && timestamp(m["review_at"].as_str())
                        .zip(timestamp(run["created_at"].as_str()))
                        .is_some_and(|(a, b)| a <= b)
            });
        ensure!(reserved, "review has no durable head reservation");
        let reviewed = result(pull, repo, review)?;
        let number = pull["number"].as_u64().context("draft number")?;
        let raw = self.fresh_draft(repo, number)?;
        ensure!(
            text(&raw, "/head/sha") == text(pull, "/headRefOid"),
            "head changed during assessment"
        );
        let (files, next) = self.get_url(
            &format!("{API_ROOT}repos/{repo}/pulls/{number}/files?per_page=100"),
            Value::clone,
            true,
        )?;
        file_veto(repo, &raw, &files, next.is_some())?;
        let (owner, name) = repo.split_once('/').context("draft repository")?;
        self.more_checks(owner, name, number, pull)?;
        let contexts = pull
            .pointer("/commits/nodes/0/commit/statusCheckRollup/contexts")
            .context("CI not read")?;
        let checks_read = contexts["nodes"].as_array().context("CI not read")?;
        ensure!(
            !checks_read.is_empty()
                && contexts.pointer("/pageInfo/hasNextPage") == Some(&Value::Bool(false))
                && contexts["totalCount"].as_u64() == Some(checks_read.len() as u64)
                && text(pull, "/commits/nodes/0/commit/oid") == text(pull, "/headRefOid"),
            "incomplete CI read"
        );
        let base: String =
            url::form_urlencoded::byte_serialize(text(pull, "/baseRefName").as_bytes()).collect();
        let rules = self.get(&format!("repos/{repo}/rules/branches/{base}"))?;
        let branch = self.get(&format!("repos/{repo}/branches/{base}"))?;
        let required = required_checks(&rules, &branch)?;
        let checks = classify_all_checks(pull, &required);
        ensure!(
            checks.pending.is_empty()
                && checks.failed.is_empty()
                && pull
                    .pointer("/commits/nodes/0/commit/statusCheckRollup/state")
                    .and_then(Value::as_str)
                    == Some("SUCCESS"),
            "CI is not complete and green"
        );
        Ok(Assessment {
            bucket: Bucket::Quick,
            assessed_head: text(pull, "/headRefOid").into(),
            reason: reviewed.reason,
            summary: reviewed.evidence,
            ci: "green".into(),
        })
    }

    fn fresh_draft(&mut self, repo: &str, number: u64) -> Result<Value> {
        self.get_url(
            &format!("{API_ROOT}repos/{repo}/pulls/{number}"),
            Value::clone,
            true,
        )
        .map(|(v, _)| v)
    }

    fn reconcile_bucket(
        &mut self,
        repo: &str,
        number: u64,
        raw: &Value,
        assessment: &Assessment,
    ) -> Result<()> {
        let labels = raw["labels"].as_array().context("draft labels not read")?;
        let has = |name: &str| labels.iter().any(|l| text(l, "/name") == name);
        let (wanted, other) = match assessment.bucket {
            Bucket::Quick => ("review:quick", "review:deep"),
            Bucket::Deep => ("review:deep", "review:quick"),
        };
        if has(wanted) && !has(other) {
            return Ok(());
        }
        if !self.config.writes_in(repo) {
            return Ok(());
        }
        if self.caps.and_then(|c| c.check(Kind::Write)).is_some() {
            return Ok(());
        }
        self.rest.start_kind("review-bucket");
        let before = self.writes;
        let written = (|| -> Result<()> {
            if self.apply.is_some() {
                // No replacement of the label set: rescue and concurrent unrelated
                // labels are independent. An interrupted pair repairs next pass.
                if has(other) {
                    self.send(
                        self.rest,
                        "DELETE",
                        &format!("repos/{repo}/issues/{number}/labels/{other}"),
                        json!({}),
                        404,
                    )?;
                }
                if !has(wanted) {
                    self.send(
                        self.rest,
                        "POST",
                        &format!("repos/{repo}/issues/{number}/labels"),
                        json!({"labels": [wanted]}),
                        0,
                    )?;
                }
            }
            Ok(())
        })();
        if (self.apply.is_none() || self.writes > before)
            && let Some(caps) = self.caps
        {
            caps.count(Kind::Write);
        }
        written
    }

    pub(super) fn draft_review(
        &mut self,
        repo: &str,
        pull: &mut Value,
        comments: &[Value],
        marker: &Marker,
        intake: bool,
    ) -> Result<(String, bool, Option<Wait>, Option<Stepped>)> {
        let number = pull["number"].as_u64().context("draft number")?;
        let head = text(pull, "/headRefOid").to_owned();
        let mut assessment = if intake && !is_held(pull) {
            self.draft_assessment(repo, pull, comments)
                .unwrap_or_else(|e| Assessment::deep(&head, format!("{e:#}")))
        } else {
            Assessment::deep(
                &head,
                "review intake bounded or held; pending fresh evidence",
            )
        };
        let raw = self.fresh_draft(repo, number)?;
        ensure!(
            raw["state"] == "open" && raw["draft"] == true,
            "draft changed before reconciliation"
        );
        if text(&raw, "/head/sha") != head {
            // Never publish old quick on a newly observed head, even when there
            // is no budget left to inspect or dispatch that head this pass.
            assessment = Assessment::deep(
                text(&raw, "/head/sha"),
                "head changed; pending fresh review",
            );
        }
        if raw["title"] != pull["title"]
            || raw["body"] != pull["body"]
            || text(&raw, "/base/ref") != text(pull, "/baseRefName")
        {
            assessment = Assessment::deep(
                text(&raw, "/head/sha"),
                "description or base changed; pending fresh review",
            );
        }
        if !is_held(pull) {
            self.reconcile_bucket(repo, number, &raw, &assessment)?;
        }
        let published = self.fresh_draft(repo, number)?;
        if text(&published, "/head/sha") != assessment.assessed_head
            || published["title"] != raw["title"]
            || published["body"] != raw["body"]
            || published["base"]["ref"] != raw["base"]["ref"]
        {
            assessment = Assessment::deep(
                text(&published, "/head/sha"),
                "head changed before publication; pending fresh review",
            );
            if !is_held(pull) {
                self.reconcile_bucket(repo, number, &published, &assessment)?;
            }
        }
        let wait = Wait {
            repository: repo.into(),
            number,
            head: assessment.assessed_head.clone(),
            kind: "promote",
            reason: "drafts to inspect and promote".into(),
            review_bucket: Some(assessment),
        };
        let intake = if intake && text(&published, "/head/sha") == head {
            self.ordinary_review(repo, pull, comments, marker)?
        } else {
            None
        };
        let bucket = wait.review_bucket.as_ref().context("draft assessment")?;
        let classification = format!(
            "{} at {}: {}",
            bucket.bucket.label(),
            bucket.assessed_head,
            bucket.reason
        );
        let line = intake
            .map(|i| format!("{}; {classification}", i.0))
            .unwrap_or_else(|| {
                format!("{repo}#{number} draft: {classification}; no promotion or merge authority")
            });
        Ok((line, false, Some(wait), None))
    }
}
