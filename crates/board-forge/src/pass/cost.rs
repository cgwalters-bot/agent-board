//! The pass's last stage, with `cost` set (docs/cost.md#publishing): from
//! `cost.hourUtc` each day, every one of the last
//! [`PUBLISH_DAYS`](crate::cost::PUBLISH_DAYS) days whose report is not on
//! `epics.repository`'s [`DATA_BRANCH`] yet is read and published there as
//! `cost/YYYY-MM-DD.json`, in one commit: yesterday's once a day, and the
//! whole week on the first run. Its reads and writes have their own request
//! bound ([`Reader`]), so that a week's backfill takes none of the other
//! stages' requests, within the pass's time bound; once that bound is spent,
//! the days read so far are published, and the rest left to the next pass.
//! One `--max-writes` step; a preview reads only what is published.
use super::*;
use crate::cost::{
    DATA_BRANCH, DATA_DIR, Reader, commit, date_text, published, read_day, repositories,
};

/// After the other stages: publishes the due days' reports, a line saying
/// what it did, and one for each day it could not read.
pub(super) fn stage<C: Rest>(
    client: &C,
    config: &Config,
    (now, apply, deadline): (OffsetDateTime, bool, Instant),
    caps: &Caps,
    report: &mut Report,
) {
    let (Some(publish), Some(epics)) = (&config.cost, &config.epics) else {
        return;
    };
    let days = publish.days(now);
    let Some(last) = days.last().copied() else {
        return;
    };
    let repository = epics.repository.as_str();
    let fail = |report: &mut Report, line: String| {
        report.failures += 1;
        report.lines.push(line);
    };
    let cache = publish.cache.as_deref().map(std::path::Path::new);
    if let Some(cache) = cache.filter(|dir| !dir.is_dir()) {
        let line = format!("cost error: cache {} is not a directory", cache.display());
        return fail(report, line);
    }
    let reader = Reader::new(client, cache).until(deadline);
    let have = match published(&reader, repository) {
        Ok(have) => have,
        Err(error) => return fail(report, format!("cost error: {error:#}")),
    };
    let due: Vec<_> = (days.into_iter())
        .map(|date| (format!("{}.json", date_text(date)), date))
        .filter(|(name, _)| !have.contains(name))
        .collect();
    if due.is_empty() {
        let line = format!(
            "cost: published through {} on {repository}@{DATA_BRANCH}",
            date_text(last)
        );
        return report.lines.push(line);
    }
    let paths: Vec<String> = due
        .iter()
        .map(|(name, _)| format!("{DATA_DIR}/{name}"))
        .collect();
    let line = format!(
        "cost: publish {} to {repository}@{DATA_BRANCH}",
        paths.join(", ")
    );
    if let Some(held) = caps.check(Kind::Write) {
        return report.lines.push(format!("{held}: {line}"));
    }
    caps.count(Kind::Write);
    if !apply {
        return report.lines.push(format!(
            "{line}; a preview reads no runs and writes nothing"
        ));
    }
    let repositories = repositories(config);
    let mut files = Vec::new();
    for ((_, date), path) in due.into_iter().zip(paths) {
        match read_day(&reader, &repositories, date) {
            Ok(day) => {
                let json = crate::cost::report(date, day, &config.prices);
                files.push((path, format!("{json:#}\n")));
            }
            Err(error) => fail(report, format!("cost error: {path}: {error:#}")),
        }
        // The days read so far are published; the next pass reads on.
        if reader.exhausted() {
            break;
        }
    }
    if files.is_empty() {
        return;
    }
    let paths: Vec<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
    let message = format!("cost: Publish {}", paths.join(", "));
    match commit(&reader, repository, &files, &message) {
        Ok(sha) => report.lines.push(format!(
            "cost: published {} to {repository}@{DATA_BRANCH} as {sha} ({} requests)",
            paths.join(", "),
            reader.spent()
        )),
        Err(error) => fail(report, format!("cost error: {error:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::tests::{Forge, REPOSITORIES};

    fn config(cost: Value) -> Config {
        let mut config = json!({
            "operator": "cgwalters", "bot": "cgwalters-bot",
            "dispatchRepo": REPOSITORIES[1], "dispatchWorkflow": "dispatch.yml",
            "workRepositories": [REPOSITORIES[0]],
            "epics": {"repository": REPOSITORIES[1]},
            "prices": {"anthropic/claude-x": {"input": 3, "cached_input": 0.3, "output": 15}},
        });
        if !cost.is_null() {
            config["cost"] = cost;
        }
        Config::parse(&config.to_string()).unwrap()
    }

    fn at(time: &str) -> OffsetDateTime {
        OffsetDateTime::parse(time, &Rfc3339).unwrap()
    }

    const AFTER: &str = "2026-10-29T06:30:00Z";
    const LISTING: &str = "GET repos/cgwalters-forge/tracker/contents/cost?ref=data";

    /// The stage's lines, failures and requests sent, `writes` its cap.
    fn stage_on(
        forge: &Forge,
        now: &str,
        apply: bool,
        writes: Option<usize>,
    ) -> (Vec<String>, usize, Vec<String>) {
        stage_with(forge, json!({"hourUtc": 6}), now, (apply, writes))
    }

    fn stage_with(
        forge: &Forge,
        cost: Value,
        now: &str,
        (apply, writes): (bool, Option<usize>),
    ) -> (Vec<String>, usize, Vec<String>) {
        let caps = Caps::new(Limits {
            writes,
            ..Limits::default()
        });
        let mut report = Report::default();
        let deadline = Instant::now() + Duration::from_secs(60);
        let within = (at(now), apply, deadline);
        stage(forge, &config(cost), within, &caps, &mut report);
        (report.lines, report.failures, forge.requests())
    }

    #[test]
    fn publishes_the_days_read_when_one_fails_and_none_after_a_rate_limit() {
        let mut forge = Forge::new();
        forge.refuse = Some(("created=2026-10-25", false));
        let (lines, failures, requests) = stage_on(&forge, AFTER, true, None);
        assert_eq!(failures, 1);
        assert!(
            lines[0].starts_with("cost error: cost/2026-10-25.json: GET repos/"),
            "{lines:?}"
        );
        assert!(lines[1].starts_with("cost: published cost/2026-10-22.json, cost/2026-10-23.json, cost/2026-10-24.json, cost/2026-10-26.json,"), "{lines:?}");
        assert_eq!(requests.len(), 5);

        let mut forge = Forge::new();
        forge.refuse = Some(("created=2026-10-22", true));
        let (lines, failures, requests) = stage_on(&forge, AFTER, true, None);
        assert_eq!(
            (failures, lines.len(), requests),
            (1, 1, vec![LISTING.to_owned()])
        );
        assert!(lines[0].contains("refused created=2026-10-22"), "{lines:?}");

        // A cache directory that is not there is an error, before any read.
        let forge = Forge::new();
        let cost = json!({"hourUtc": 6, "cache": "/nonexistent/cost-cache"});
        let (lines, failures, requests) = stage_with(&forge, cost, AFTER, (true, None));
        assert_eq!(
            (lines, failures, requests.len()),
            (
                vec!["cost error: cache /nonexistent/cost-cache is not a directory".to_owned()],
                1,
                0
            )
        );
    }

    #[test]
    fn backfills_a_week_into_a_new_branch_in_one_commit() {
        let forge = Forge::new();
        let (lines, failures, requests) = stage_on(&forge, AFTER, true, None);
        assert_eq!(failures, 0, "{lines:?}");
        let days: Vec<String> = (22..=28)
            .map(|d| format!("cost/2026-10-{d}.json"))
            .collect();
        assert_eq!(
            lines,
            [format!(
                "cost: published {} to cgwalters-forge/tracker@data as commit1 (25 requests)",
                days.join(", ")
            )]
        );
        let api = "repos/cgwalters-forge/tracker/git";
        assert_eq!(
            requests,
            [
                LISTING.to_owned(),
                format!("GET {api}/ref/heads/data"),
                format!("POST {api}/trees"),
                format!("POST {api}/commits"),
                format!("POST {api}/refs"),
            ]
        );
        let sent = forge.sent.borrow();
        let tree = &sent[2].2;
        assert_eq!(tree.get("base_tree"), None);
        let paths: Vec<&str> = (tree["tree"].as_array().unwrap().iter())
            .map(|e| e["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, days);
        let last: Value =
            serde_json::from_str(tree["tree"][6]["content"].as_str().unwrap()).unwrap();
        assert_eq!(last["totals"]["usd"], 1.02);
        assert_eq!(sent[3].2["parents"], json!([]));
        assert_eq!(
            sent[4].2,
            json!({"ref": "refs/heads/data", "sha": "commit1"})
        );
    }

    #[test]
    fn publishes_yesterday_onto_the_branch_once_after_the_hour() {
        let mut forge = Forge::new();
        forge.branch = Some("head0");
        forge.published = (22..=27).map(|d| format!("2026-10-{d}.json")).collect();
        let (lines, failures, requests) = stage_on(&forge, AFTER, true, None);
        assert_eq!(failures, 0, "{lines:?}");
        assert_eq!(
            lines,
            [
                "cost: published cost/2026-10-28.json to cgwalters-forge/tracker@data as commit1 (14 requests)"
            ]
        );
        let api = "repos/cgwalters-forge/tracker/git";
        assert_eq!(
            requests[1..],
            [
                format!("GET {api}/ref/heads/data"),
                format!("GET {api}/commits/head0"),
                format!("POST {api}/trees"),
                format!("POST {api}/commits"),
                format!("PATCH {api}/refs/heads/data"),
            ]
        );
        let sent = forge.sent.borrow();
        assert_eq!(sent[3].2["base_tree"], "tree0");
        assert_eq!(sent[4].2["parents"], json!(["head0"]));
        assert_eq!(sent[5].2, json!({"sha": "commit1", "force": false}));
        drop(sent);

        // Once it is there, a pass reads only the listing.
        forge.published.push("2026-10-28.json".into());
        forge.sent.borrow_mut().clear();
        let (lines, _, requests) = stage_on(&forge, AFTER, true, None);
        assert_eq!(
            lines,
            ["cost: published through 2026-10-28 on cgwalters-forge/tracker@data"]
        );
        assert_eq!(requests, [LISTING]);
    }

    #[test]
    fn waits_for_the_hour_and_previews_or_holds_without_reading_runs() {
        let days: Vec<String> = (22..=28)
            .map(|d| format!("cost/2026-10-{d}.json"))
            .collect();
        let publish = format!(
            "cost: publish {} to cgwalters-forge/tracker@data",
            days.join(", ")
        );
        let held = vec![format!("held by --max-writes: {publish}")];
        let preview = vec![format!(
            "{publish}; a preview reads no runs and writes nothing"
        )];
        let cases = [
            ("2026-10-29T05:59:59Z", true, None, vec![]),
            (AFTER, false, None, preview),
            (AFTER, true, Some(0), held.clone()),
            (AFTER, false, Some(0), held),
        ];
        for (now, apply, writes, expected) in cases {
            let forge = Forge::new();
            let (lines, failures, requests) = stage_on(&forge, now, apply, writes);
            assert_eq!(lines, expected, "{now} {apply}");
            assert_eq!(failures, 0);
            let read = if expected.is_empty() {
                vec![]
            } else {
                vec![LISTING]
            };
            assert_eq!(requests, read, "{now} {apply}");
            assert_eq!(forge.downloads.get(), 0);
        }
    }

    #[test]
    fn the_configuration_is_checked() {
        let base = config(Value::Null);
        assert!(base.cost.is_none());
        for (cost, error) in [
            (json!({"hourUtc": 24}), "cost hourUtc is not 0 to 23"),
            (
                json!({"hourUtc": 6, "branch": "main"}),
                "unknown field `branch`",
            ),
        ] {
            let mut value = json!({
                "operator": "cgwalters", "bot": "cgwalters-bot",
                "dispatchRepo": "o/tracker", "dispatchWorkflow": "dispatch.yml",
                "epics": {"repository": "o/tracker"}, "cost": cost,
            });
            let got = format!("{:#}", Config::parse(&value.to_string()).unwrap_err());
            assert!(got.contains(error), "{got}");
            value.as_object_mut().unwrap().remove("epics");
            value["cost"] = json!({"hourUtc": 6});
            let got = format!("{:#}", Config::parse(&value.to_string()).unwrap_err());
            assert!(got.contains("cost publishes to epics.repository"), "{got}");
        }
        let prices = json!({
            "operator": "cgwalters", "bot": "cgwalters-bot",
            "dispatchRepo": "o/tracker", "dispatchWorkflow": "dispatch.yml",
            "prices": {"m": {"input": 1, "cached_input": 1, "output": -1}},
        });
        let got = format!("{:#}", Config::parse(&prices.to_string()).unwrap_err());
        assert!(got.contains("prices.m.output is not 0 to"), "{got}");
    }
}
