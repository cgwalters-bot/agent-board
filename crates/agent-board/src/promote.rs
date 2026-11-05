//! Standalone promotion; preview by default, publication explicitly opted in.
use anyhow::{Context, Result, ensure};
use board_forge::{
    github::Client,
    pass::{Config, PASS_SECONDS_LIMIT, Recorder, Replay},
    promote,
};
use std::{
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(clap::Args)]
pub struct Promote {
    /// Publish at most one approved candidate; also requires promote.publish.
    #[arg(long, conflicts_with_all = ["record", "replay"], requires = "max_writes")]
    pub apply: bool,
    /// Whole-step publication cap (three writes reserved per candidate).
    #[arg(long, requires = "apply")]
    pub max_writes: Option<usize>,
    /// Trusted shared pass configuration; promote.forks absent or empty means off, with no forge reads or token lookup. The pass remains inert. With forks, GH_TOKEN reads within the pass's request/time bounds and ten 100-row pages per listing.
    #[arg(long, value_name = "FILE")]
    pub config: PathBuf,
    /// Save live read responses projected to the evidence fields used here for offline replay. No forge writes. Record public repositories only before publishing the file; comment bodies remain in it.
    #[arg(long, value_name = "FILE", conflicts_with = "replay")]
    pub record: Option<PathBuf>,
    /// Preview recorded reads at their recorded clock, without credentials.
    #[arg(long, value_name = "FILE")]
    pub replay: Option<PathBuf>,
}

pub fn run(args: &Promote, out: &mut impl Write) -> Result<()> {
    let read = |path: &PathBuf| {
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
    };
    let config = Config::parse(&read(&args.config)?)?;
    ensure!(
        config.forgejo_board().is_none(),
        "promote supports GitHub only"
    );
    if args.apply && promote::enabled(&config) {
        ensure!(
            config.promote.as_ref().is_some_and(|p| p.publish),
            "promote.publish must be true for --apply"
        );
    }
    let replay = args
        .replay
        .as_ref()
        .map(|path| -> Result<Replay> { Replay::from_json(&serde_json::from_str(&read(path)?)?) })
        .transpose()?;
    let lines = if !promote::enabled(&config) {
        if let Some(path) = &args.record {
            let now = time::OffsetDateTime::now_utc();
            let empty = Replay {
                now,
                requests: vec![],
            };
            std::fs::write(path, Recorder::new(&empty).fixture(now)?)
                .with_context(|| format!("write {}", path.display()))?;
        }
        vec!["promote: off".to_owned()]
    } else if let Some(replay) = &replay {
        promote::preview(replay, &config, replay.now)?
    } else {
        let now = time::OffsetDateTime::now_utc();
        let client = Client::from_env()?
            .retry_within(Instant::now() + Duration::from_secs(PASS_SECONDS_LIMIT));
        if let Some(path) = &args.record {
            let recorder = Recorder::new(&client);
            let result = promote::preview(&recorder, &config, now);
            std::fs::write(path, recording(&recorder.fixture(now)?)?)
                .with_context(|| format!("write {}", path.display()))?;
            result?
        } else if args.apply {
            promote::apply(
                &client,
                &config,
                now,
                args.max_writes.context("--max-writes required")?,
            )?
        } else {
            promote::preview(&client, &config, now)?
        }
    };
    for line in lines {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// Keep the observed fields this evaluator uses, not avatars, descriptions,
/// unrelated PR text or review bodies. This is a projection, not scenario data.
fn recording(fixture: &str) -> Result<String> {
    let mut value: serde_json::Value = serde_json::from_str(fixture)?;
    fn project(value: &mut serde_json::Value, comments: bool) {
        match value {
            serde_json::Value::Array(rows) => rows.iter_mut().for_each(|v| project(v, comments)),
            serde_json::Value::Object(object) => {
                object.retain(|key, _| {
                    [
                        "full_name",
                        "fork",
                        "parent",
                        "source",
                        "owner",
                        "login",
                        "user",
                        "number",
                        "head",
                        "base",
                        "repo",
                        "ref",
                        "sha",
                        "state",
                        "draft",
                        "labels",
                        "name",
                        "id",
                        "created_at",
                        "updated_at",
                        "submitted_at",
                        "timestamp",
                        "before",
                        "after",
                        "activity_type",
                        "actor",
                        "title",
                        "body",
                        "merged_at",
                        "ahead_by",
                        "behind_by",
                        "total_commits",
                        "merge_base_commit",
                        "commits",
                        "commit",
                        "message",
                        "parents",
                        "author",
                        "protected",
                        "protection",
                        "enabled",
                        "required_status_checks",
                        "checks",
                        "contexts",
                        "context",
                        "app_id",
                        "integration_id",
                        "enforcement_level",
                        "type",
                        "parameters",
                        "check_runs",
                        "total_count",
                        // The policy gate: record files, listings and history.
                        "path",
                        "size",
                        "encoding",
                        "content",
                        "committer",
                        "verification",
                        "verified",
                        "default_branch",
                        "merge_commit_sha",
                        "commit_id",
                        "tree",
                    ]
                    .contains(&key.as_str())
                        || (comments && key == "body")
                });
                object.values_mut().for_each(|v| project(v, comments));
            }
            _ => {}
        }
    }
    for entry in value["requests"]
        .as_array_mut()
        .context("recorded requests")?
    {
        let comments = entry["request"]["get"]
            .as_str()
            .is_some_and(|s| s.contains("/comments?"));
        if let Some(body) = entry["answer"].get_mut("body") {
            project(body, comments);
        }
    }
    Ok(serde_json::to_string_pretty(&value)?)
}
