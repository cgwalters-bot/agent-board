//! Weekly material and its issue, ported from tracker weekly-blog.yml at 3d010f1.
use anyhow::{Context, Result, bail, ensure};
use board_forge::github::{GitHub, Transport};
use board_forge::poll::{Rest, validate_repository, validate_workflow};
use clap::Args;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{io::Write, path::PathBuf};
use time::{Duration, OffsetDateTime, Time, UtcOffset, format_description::well_known::Rfc3339};

const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 10;
const MAX_BODY_BYTES: usize = 56_000;
const MAX_RELEASE_BYTES: usize = 10_000;

#[derive(Args)]
pub struct Digest {
    /// Trusted caller-owned JSON settings; see docs/digest.md.
    #[arg(long, value_name = "FILE")]
    config: PathBuf,
    /// Before Sunday 18:00 UTC, file the partial week up to now.
    #[arg(long)]
    partial: bool,
    /// Print the body and intended writes without making them (the default).
    #[arg(long, conflicts_with = "apply")]
    dry_run: bool,
    /// Create the issue, put it on the board and dispatch, with GH_TOKEN; DISPATCH_TOKEN starts the run if set.
    #[arg(long)]
    apply: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
    owner: String,
    project: u64,
    operator: String,
    bot: String,
    follow: Vec<String>,
    site_repository: String,
    title: String,
    brief: String,
    task: String,
    priority: String,
    changes_chars: usize,
    release_bytes: usize,
    body_bytes: usize,
    dispatch_repository: String,
    dispatch_workflow: String,
    dispatch_ref: String,
}

impl Config {
    fn validate(&self) -> Result<()> {
        for login in [&self.owner, &self.operator, &self.bot] {
            ensure!(
                board_forge::github::is_github_login(login),
                "invalid login {login}"
            );
        }
        ensure!(self.project > 0, "project must be positive");
        ensure!(self.follow.len() <= 50, "at most 50 followed repositories");
        for repo in self
            .follow
            .iter()
            .chain([&self.site_repository, &self.dispatch_repository])
        {
            validate_repository(repo)?;
        }
        validate_workflow(&self.dispatch_workflow)?;
        ensure!(!self.dispatch_ref.is_empty(), "dispatchRef is empty");
        ensure!(
            !self.title.is_empty() && !self.brief.is_empty() && !self.task.is_empty(),
            "title, brief and task are required"
        );
        ensure!(!self.priority.is_empty(), "priority is required");
        ensure!(
            (1..=MAX_BODY_BYTES).contains(&self.body_bytes),
            "bodyBytes must be 1..={MAX_BODY_BYTES}"
        );
        ensure!(
            (1..=MAX_RELEASE_BYTES).contains(&self.release_bytes),
            "releaseBytes must be 1..={MAX_RELEASE_BYTES}"
        );
        ensure!(
            (1..=MAX_BODY_BYTES).contains(&self.changes_chars),
            "changesChars must be 1..={MAX_BODY_BYTES}"
        );
        Ok(())
    }
}

struct Window {
    week: String,
    since: String,
    until: String,
    partial: bool,
}

impl Window {
    fn new(now: OffsetDateTime, partial: bool) -> Result<Self> {
        let now = now.to_offset(UtcOffset::UTC).replace_nanosecond(0)?;
        let sunday =
            now.date() - Duration::days(i64::from(now.weekday().number_days_from_sunday()));
        let end = sunday.with_time(Time::from_hms(18, 0, 0)?).assume_utc();
        ensure!(
            now >= end || partial,
            "the week ending {sunday} is not over until {}; use --partial",
            end.format(&Rfc3339)?
        );
        Ok(Self {
            week: sunday.to_string(),
            since: (end - Duration::days(7)).format(&Rfc3339)?,
            until: now.min(end).format(&Rfc3339)?,
            partial: now < end,
        })
    }

    fn expand(&self, template: &str) -> String {
        template
            .replace("{week}", &self.week)
            .replace("{since}", &self.since)
            .replace("{until}", &self.until)
    }

    fn contains(&self, date: &str) -> bool {
        date >= self.since.as_str() && date < self.until.as_str()
    }
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("missing string {key}"))
}

fn get(api: &impl Rest, path: &str) -> Result<Value> {
    let response = api.get(&format!("https://api.github.com/{path}"), None)?;
    ensure!(
        response.status == 200,
        "GET {path}: HTTP {}",
        response.status
    );
    Ok(response.body)
}

/// A full tenth page cannot establish completeness. Refuse rather than omit data.
fn listing(api: &impl Rest, path: &str) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for page in 1..=MAX_PAGES {
        let separator = if path.ends_with('?') { "" } else { "&" };
        let body = get(
            api,
            &format!("{path}{separator}per_page={PAGE_SIZE}&page={page}"),
        )?;
        let batch = body.as_array().context("listing is not an array")?;
        ensure!(batch.len() <= PAGE_SIZE, "oversized listing page");
        rows.extend(batch.iter().cloned());
        if batch.len() < PAGE_SIZE {
            return Ok(rows);
        }
    }
    bail!("incomplete listing for {path}: reaches 1,000 entries")
}

fn releases(api: &impl Rest, config: &Config, window: &Window) -> Result<Vec<Value>> {
    let mut releases = Vec::new();
    for repo in &config.follow {
        let visibility = get(api, &format!("repos/{repo}"))?;
        if text(&visibility, "visibility")? != "public" {
            continue;
        }
        for release in listing(api, &format!("repos/{repo}/releases?"))? {
            if release["draft"] == false
                && release["published_at"]
                    .as_str()
                    .is_some_and(|date| window.contains(date))
            {
                releases.push(json!({"repository":repo, "tag":text(&release,"tag_name")?,
                    "name":release["name"].as_str().unwrap_or(""), "url":text(&release,"html_url")?,
                    "published_at":text(&release,"published_at")?}));
            }
        }
    }
    releases.sort_by(|a, b| {
        ["repository", "published_at", "tag"]
            .map(|key| a[key].as_str().unwrap_or(""))
            .cmp(&["repository", "published_at", "tag"].map(|key| b[key].as_str().unwrap_or("")))
    });
    Ok(releases)
}

fn release_section(rows: &[Value], left: usize) -> Result<String> {
    let mut result = "\n## Releases published in the window\n\n".to_owned();
    if left > 0 {
        result += &format!("_{left} releases left out to fit the release material budget._\n\n");
    } else if rows.is_empty() {
        result += "_None were published this week._\n";
    }
    for row in rows {
        let fields = ["repository", "tag", "name", "url", "published_at"]
            .map(|key| text(row, key).map(|s| s.replace(['\r', '\n'], " ")));
        let fields = fields.into_iter().collect::<Result<Vec<_>>>()?;
        result += &format!("- {}\n", fields.join(" | "));
    }
    Ok(result)
}

fn strip_comments(body: &str) -> String {
    let mut rest = body;
    let mut result = String::new();
    while let Some(start) = rest.find("<!--") {
        let Some(end) = rest[start + 4..].find("-->") else {
            break;
        };
        result += &rest[..start];
        rest = &rest[start + 4 + end + 3..];
    }
    result += rest;
    result.trim_end().to_owned()
}

fn update_section(rows: &[Value], left: usize, project_url: &str) -> Result<String> {
    let mut result = "\n## Project status updates, oldest first\n\n".to_owned();
    if left > 0 {
        result += &if rows.is_empty() {
            format!(
                "_All {left} status updates of the week are left out to fit the issue body limit; they are on the project, {project_url}._\n"
            )
        } else {
            format!(
                "_The {left} oldest status updates of the week are left out to fit the issue body limit; they are on the project, {project_url}._\n\n"
            )
        };
    } else if rows.is_empty() {
        result += "_None were posted this week._\n";
    }
    let mut separator = "";
    for row in rows {
        result += &format!(
            "{separator}### {}, {}\n\n{}\n",
            text(row, "createdAt")?,
            row["status"].as_str().unwrap_or("no status"),
            strip_comments(row["body"].as_str().unwrap_or(""))
        );
        separator = "\n";
    }
    Ok(result)
}

fn compose(
    config: &Config,
    window: &Window,
    changes: &str,
    releases: &[Value],
    updates: &[Value],
) -> Result<String> {
    let mut release_material = None;
    for n in (0..=releases.len()).rev() {
        let section = release_section(&releases[..n], releases.len() - n)?;
        if section.len() <= config.release_bytes {
            release_material = Some(section);
            break;
        }
    }
    let release_material =
        release_material.context("release omission note exceeds releaseBytes")?;
    let mut brief = window.expand(&config.brief);
    if window.partial {
        brief += &format!(
            " _Partial: filed before the end of the week, with what there was at {}._",
            window.until
        );
    }
    let top = format!(
        "{brief}\n\n## Changes from {} to {}\n\n{changes}{release_material}",
        window.since, window.until
    );
    let mut updates: Vec<_> = updates
        .iter()
        .filter(|row| {
            row["createdAt"]
                .as_str()
                .is_some_and(|date| window.contains(date))
        })
        .cloned()
        .collect();
    updates.sort_by(|a, b| a["createdAt"].as_str().cmp(&b["createdAt"].as_str()));
    let project = format!(
        "https://github.com/orgs/{}/projects/{}",
        config.owner, config.project
    );
    for dropped in 0..=updates.len() {
        let body = format!(
            "{top}{}\n",
            update_section(&updates[dropped..], dropped, &project)?
        );
        if body.len() <= config.body_bytes {
            return Ok(body);
        }
    }
    bail!(
        "the changes, releases and omission note exceed {} bytes",
        config.body_bytes
    )
}

fn find_issue<'a>(rows: &'a [Value], title: &str, bot: &str) -> Option<&'a Value> {
    rows.iter()
        .filter(|row| {
            row["pull_request"].is_null() && row["title"] == title && row["user"]["login"] == bot
        })
        .min_by_key(|row| row["number"].as_u64().unwrap_or(u64::MAX))
}

fn active(runs: &[Value], target: &str) -> bool {
    runs.iter().any(|run| {
        run["status"] != "completed"
            && run["display_title"]
                .as_str()
                .is_some_and(|title| title == target || title.starts_with(&format!("{target} ")))
    })
}

pub fn run(args: &Digest, out: &mut impl Write) -> Result<()> {
    let config: Config =
        serde_json::from_str(&std::fs::read_to_string(&args.config).context("read digest config")?)
            .context("decode digest config")?;
    config.validate()?;
    let window = Window::new(OffsetDateTime::now_utc(), args.partial)?;
    let client = board_forge::github::Client::from_env()?;
    let dispatch = board_forge::github::Client::from_var("DISPATCH_TOKEN");
    execute(
        &config,
        &window,
        &client,
        dispatch.as_ref().unwrap_or(&client),
        args.apply,
        out,
    )
}

fn execute<C: Rest + Transport>(
    config: &Config,
    window: &Window,
    client: &C,
    dispatch: &C,
    apply: bool,
    out: &mut impl Write,
) -> Result<()> {
    let github = GitHub {
        owner: config.owner.clone(),
        number: config.project,
        transport: board_forge::github::Blocking(client),
    };
    let changes = github
        .changes(
            board_forge::changes::parse_time(&window.since)?,
            Some(board_forge::changes::parse_time(&window.until)?),
            Some(&config.operator),
            true,
        )?
        .markdown_within(config.changes_chars)?;
    let releases = releases(client, config, window)?;
    let updates = client.graphql("query($owner:String!,$number:Int!){organization(login:$owner){projectV2(number:$number){statusUpdates(first:100,orderBy:{field:CREATED_AT,direction:DESC}){nodes{createdAt status body}}}}}", json!({"owner":config.owner,"number":config.project}))?;
    let updates = updates
        .pointer("/organization/projectV2/statusUpdates/nodes")
        .and_then(Value::as_array)
        .context("missing status updates")?;
    let body = compose(config, window, &changes, &releases, updates)?;
    publish(config, window, client, dispatch, apply, &body, out)
}

fn publish<C: Rest + Transport>(
    config: &Config,
    window: &Window,
    client: &C,
    dispatch: &C,
    apply: bool,
    body: &str,
    out: &mut impl Write,
) -> Result<()> {
    let github = GitHub {
        owner: config.owner.clone(),
        number: config.project,
        transport: board_forge::github::Blocking(client),
    };
    writeln!(out, "{body}")?;
    let title = window.expand(&config.title);
    let issues = listing(
        client,
        &format!(
            "repos/{}/issues?state=all&creator={}",
            config.site_repository, config.bot
        ),
    )?;
    let found = find_issue(&issues, &title, &config.bot);
    let mut number = None;
    if let Some(issue) = found {
        let n = issue["number"].as_u64().context("missing issue number")?;
        if text(issue, "state")? == "closed" {
            writeln!(
                out,
                "{}#{n} is closed; nothing to start",
                config.site_repository
            )?;
            return Ok(());
        }
        let (owner, name) = config
            .site_repository
            .split_once('/')
            .context("invalid repository")?;
        let closing = client.graphql("query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){issue(number:$number){closedByPullRequestsReferences(first:1,includeClosedPrs:false){totalCount}}}}", json!({"owner":owner,"name":name,"number":n}))?;
        let count = closing
            .pointer("/repository/issue/closedByPullRequestsReferences/totalCount")
            .and_then(Value::as_u64)
            .context("missing closing pull request count")?;
        if count > 0 {
            writeln!(
                out,
                "{}#{n} has an open closing pull request; nothing to start",
                config.site_repository
            )?;
            return Ok(());
        }
        writeln!(
            out,
            "Reuse {}#{n}; leave its body unchanged",
            config.site_repository
        )?;
        number = Some(n);
    } else {
        let request = json!({"title":title,"body":body});
        writeln!(
            out,
            "POST repos/{}/issues {request}",
            config.site_repository
        )?;
        if apply {
            let created = send(
                client,
                &format!("repos/{}/issues", config.site_repository),
                &request,
            )?;
            number = Some(
                created["number"]
                    .as_u64()
                    .context("created issue has no number")?,
            );
        }
    }
    let item = number.map_or_else(|| "<new issue number>".to_owned(), |n| n.to_string());
    writeln!(
        out,
        "Board {}/{}: add {}#{item}, Priority={}",
        config.owner, config.project, config.site_repository, config.priority
    )?;
    if let Some(n) = number {
        let identity = crate::item::issue_identity(
            &format!("{}#{n}", config.site_repository),
            None,
            "github.com",
        )?;
        if apply {
            crate::item::write(
                &github,
                &identity,
                &[format!("Priority={}", config.priority)],
                crate::item::Mode::Add,
                true,
                &mut None,
                out,
            )?;
        }
        let runs = get(
            dispatch,
            &format!(
                "repos/{}/actions/workflows/{}/runs?per_page=100",
                config.dispatch_repository, config.dispatch_workflow
            ),
        )?;
        let runs = runs["workflow_runs"]
            .as_array()
            .context("missing workflow runs")?;
        if active(runs, &format!("implement {}#{n}", config.site_repository)) {
            writeln!(out, "Dispatch already queued or running; nothing to start")?;
            return Ok(());
        }
    }
    let task = window.expand(&config.task).replace("{issue}", &item);
    let request = json!({"ref":config.dispatch_ref,"inputs":{"kind":"implement","repo":config.site_repository,"item":item,"task":task}});
    let path = format!(
        "repos/{}/actions/workflows/{}/dispatches",
        config.dispatch_repository, config.dispatch_workflow
    );
    writeln!(out, "POST {path} {request}")?;
    if apply {
        send(dispatch, &path, &request)?;
    }
    Ok(())
}

fn send(api: &impl Rest, path: &str, body: &Value) -> Result<Value> {
    let (status, value) = api.send("POST", &format!("https://api.github.com/{path}"), body)?;
    ensure!((200..300).contains(&status), "POST {path}: HTTP {status}");
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use board_forge::poll::RestResponse;
    use std::cell::RefCell;

    fn settings() -> Config {
        serde_json::from_str(include_str!("../../../fixtures/digest/config.json")).unwrap()
    }

    fn window() -> Window {
        Window::new(
            board_forge::changes::parse_time("2026-10-25T18:00:00Z").unwrap(),
            false,
        )
        .unwrap()
    }

    struct Api {
        issues: Value,
        runs: Value,
        closing: u64,
        page_size: usize,
        fail: bool,
        allow_write: bool,
        writes: RefCell<Vec<Value>>,
        calls: RefCell<Vec<String>>,
    }

    impl Default for Api {
        fn default() -> Self {
            Self {
                issues: json!([]),
                runs: json!([]),
                closing: 0,
                page_size: 0,
                fail: false,
                allow_write: false,
                writes: RefCell::new(vec![]),
                calls: RefCell::new(vec![]),
            }
        }
    }

    impl Rest for Api {
        fn get(&self, url: &str, _: Option<(&str, &str)>) -> Result<RestResponse> {
            self.calls.borrow_mut().push(url.to_owned());
            ensure!(!self.fail, "API failure");
            let body = if url.contains("/issues?") {
                self.issues.clone()
            } else if url.contains("/runs?") {
                json!({"workflow_runs":self.runs})
            } else if url.contains("/releases?") {
                if self.page_size > 0 {
                    Value::Array(vec![
                        json!({"draft":false,"published_at":null});
                        self.page_size
                    ])
                } else if url.ends_with("page=1") {
                    serde_json::from_str(include_str!("../../../fixtures/digest/releases.json"))?
                } else {
                    json!([])
                }
            } else {
                json!({"visibility":if url.ends_with("/private") { "private" } else { "public" }})
            };
            Ok(RestResponse {
                status: 200,
                etag: None,
                last_modified: None,
                link: None,
                body,
            })
        }

        fn send(&self, method: &str, path: &str, body: &Value) -> Result<(u16, Value)> {
            assert!(self.allow_write, "preview must never write");
            assert_eq!(method, "POST");
            self.writes
                .borrow_mut()
                .push(json!({"path":path,"body":body}));
            Ok((201, json!({"number":7})))
        }
    }

    impl Transport for Api {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            if query.contains("mutation") {
                assert!(self.allow_write, "preview must never write");
                self.writes.borrow_mut().push(variables.clone());
                if query.contains("addProjectV2ItemById") {
                    return Ok(json!({"addProjectV2ItemById":{"item":{"id":"M"}}}));
                }
                assert_eq!(variables["value"]["singleSelectOptionId"], "p0");
                return Ok(
                    json!({"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":"M","fieldValueByName":{"optionId":"p0"}}}}),
                );
            }
            if query.contains("repositoryOwner(login:") {
                return Ok(
                    json!({"repositoryOwner":{"projectV2":{"id":"P","url":"https://github.com/orgs/example/projects/1"}}}),
                );
            }
            if query.contains("fields(first") {
                return Ok(
                    json!({"node":{"fields":{"nodes":[{"id":"F","name":"Priority","dataType":"SINGLE_SELECT","options":[{"id":"p0","name":"P0"}]}],"pageInfo":{"hasNextPage":false}}}}),
                );
            }
            if query.contains("projectItems(first") {
                return Ok(
                    json!({"repository":{"issue":{"id":"I","number":7,"repository":{"nameWithOwner":"example/site"},"projectItems":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}),
                );
            }
            Ok(
                json!({"repository":{"issue":{"closedByPullRequestsReferences":{"totalCount":self.closing}}}}),
            )
        }
    }

    #[test]
    fn weekly_material_tables() {
        let config = settings();
        config.validate().unwrap();
        let window = window();
        let api = Api::default();
        let selected = releases(&api, &config, &window).unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|r| r["tag"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["v0.6.3", "v0.6.4"]
        );
        assert!(
            !api.calls
                .borrow()
                .iter()
                .any(|url| url.contains("private/releases"))
        );
        let updates: Vec<Value> =
            serde_json::from_str(include_str!("../../../fixtures/digest/updates.json")).unwrap();
        for (name, rs, us, limit, expected) in [
            (
                "empty week",
                vec![],
                vec![],
                MAX_BODY_BYTES,
                "_None were published this week._",
            ),
            (
                "recorded week",
                selected.clone(),
                updates.clone(),
                MAX_BODY_BYTES,
                "Visible progress",
            ),
            (
                "over budget week",
                selected.clone(),
                vec![json!({"createdAt":window.since,"body":"界".repeat(20_000)})],
                MAX_BODY_BYTES,
                "_All 1 status updates",
            ),
            (
                "oldest first dropped",
                vec![],
                vec![
                    json!({"createdAt":window.since,"body":"界".repeat(20_000)}),
                    updates[1].clone(),
                ],
                MAX_BODY_BYTES,
                "_The 1 oldest status updates",
            ),
        ] {
            let config = Config {
                body_bytes: limit,
                ..settings()
            };
            let body = compose(&config, &window, "Changes\n", &rs, &us).unwrap();
            assert!(body.len() <= limit, "{name}");
            assert!(body.contains(expected), "{name}: {body}");
            assert!(!body.contains("hidden"), "{name}");
            assert!(body.find("## Changes") < body.find("## Releases"));
            assert!(body.find("## Releases") < body.find("## Project status"));
        }
        let huge = vec![
            json!({"repository":"example/tool","published_at":window.since,"tag":"huge","url":"https://example.com","name":"界".repeat(4000)}),
        ];
        let body = compose(&config, &window, "", &huge, &[]).unwrap();
        assert!(body.contains("1 releases left out"));
        let releases = body
            .split("\n## Releases")
            .nth(1)
            .unwrap()
            .split("\n## Project")
            .next()
            .unwrap();
        assert!(releases.len() + "\n## Releases".len() <= MAX_RELEASE_BYTES);
        let body = compose(&config, &window, "", &[], &[]).unwrap();
        let exact = Config {
            body_bytes: body.len(),
            ..settings()
        };
        assert_eq!(compose(&exact, &window, "", &[], &[]).unwrap(), body);
        assert!(
            compose(
                &Config {
                    body_bytes: body.len() - 1,
                    ..settings()
                },
                &window,
                "",
                &[],
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn windows_and_partial_table() {
        for (now, partial, week, until, marked) in [
            (
                "2026-10-25T17:59:59Z",
                true,
                "2026-10-25",
                "2026-10-25T17:59:59Z",
                true,
            ),
            (
                "2026-10-25T18:00:00Z",
                false,
                "2026-10-25",
                "2026-10-25T18:00:00Z",
                false,
            ),
            (
                "2026-10-28T12:00:00Z",
                false,
                "2026-10-25",
                "2026-10-25T18:00:00Z",
                false,
            ),
        ] {
            let window =
                Window::new(board_forge::changes::parse_time(now).unwrap(), partial).unwrap();
            assert_eq!(window.week, week);
            assert_eq!(window.until, until);
            assert_eq!(window.partial, marked);
            let body = compose(&settings(), &window, "", &[], &[]).unwrap();
            assert_eq!(body.contains("_Partial:"), marked);
        }
        assert!(
            Window::new(
                board_forge::changes::parse_time("2026-10-25T17:59:59Z").unwrap(),
                false
            )
            .is_err()
        );
    }

    #[test]
    fn listing_bounds_and_failures() {
        for (size, fail, ok) in [
            (0, false, true),
            (100, false, false),
            (101, false, false),
            (0, true, false),
        ] {
            let api = Api {
                page_size: size,
                fail,
                ..Api::default()
            };
            assert_eq!(releases(&api, &settings(), &window()).is_ok(), ok);
            assert!(api.calls.borrow().len() <= MAX_PAGES + 2);
        }
    }

    #[test]
    fn issue_and_dispatch_tables() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../../fixtures/digest/issues.json")).unwrap();
        for case in cases {
            let api = Api {
                issues: case["issues"].clone(),
                closing: case["closing"].as_u64().unwrap(),
                runs: case["runs"].clone(),
                ..Api::default()
            };
            let mut output = Vec::new();
            publish(
                &settings(),
                &window(),
                &api,
                &api,
                false,
                "body",
                &mut output,
            )
            .unwrap();
            let output = String::from_utf8(output).unwrap();
            for expected in case["contains"].as_array().unwrap() {
                assert!(
                    output.contains(expected.as_str().unwrap()),
                    "{}: {output}",
                    case["name"]
                );
            }
            assert_eq!(
                output.contains("/dispatches"),
                case["dispatch"].as_bool().unwrap(),
                "{}: {output}",
                case["name"]
            );
            assert_eq!(
                output.contains("Priority=P0"),
                case["board"].as_bool().unwrap(),
                "{}: {output}",
                case["name"]
            );
        }
        for (status, title, want) in [
            ("queued", "implement example/site#7", true),
            ("in_progress", "implement example/site#7 extra", true),
            ("waiting", "implement example/site#7", true),
            ("completed", "implement example/site#7", false),
            ("queued", "implement example/site#70", false),
        ] {
            assert_eq!(
                active(
                    &[json!({"status":status,"display_title":title})],
                    "implement example/site#7"
                ),
                want
            );
        }
    }

    #[test]
    fn apply_writes_and_stop_gates() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../../fixtures/digest/issues.json")).unwrap();
        for case in cases {
            let api = Api {
                issues: case["issues"].clone(),
                closing: case["closing"].as_u64().unwrap(),
                runs: case["runs"].clone(),
                allow_write: true,
                ..Api::default()
            };
            publish(
                &settings(),
                &window(),
                &api,
                &api,
                true,
                "original body",
                &mut Vec::new(),
            )
            .unwrap();
            let writes = api.writes.borrow();
            let creates = usize::from(case["issues"].as_array().unwrap().is_empty());
            let board_writes = if case["board"] == true { 2 } else { 0 };
            let starts = usize::from(case["dispatch"] == true);
            assert_eq!(
                writes.len(),
                creates + board_writes + starts,
                "{}: {writes:?}",
                case["name"]
            );
            if creates > 0 {
                assert_eq!(writes[0]["body"]["body"], "original body");
            }
            if starts > 0 {
                let dispatch = &writes.last().unwrap()["body"];
                assert_eq!(dispatch["ref"], "main");
                assert_eq!(dispatch["inputs"]["item"], "7");
                assert_eq!(dispatch["inputs"]["repo"], "example/site");
                assert!(
                    dispatch["inputs"]["task"]
                        .as_str()
                        .unwrap()
                        .contains("closes #7")
                );
            }
        }
    }
}
