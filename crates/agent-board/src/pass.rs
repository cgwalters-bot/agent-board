//! `agent-board pass` (docs/pass.md): the scheduled pass, previewed or applied.
use anyhow::{Context, Result, bail, ensure};
use board_forge::caps::Limits;
use board_forge::github::Client;
use board_forge::github::{GraphQl, Transport};
use board_forge::pass::{self, Config, Mode, PassRun, Recorder, Replay, Report, Seeded};
use board_forge::poll::{Rest, State, bounds::FixBoundsSet};
use std::time::{Duration, Instant};
use std::{io::Write, path::PathBuf};
use tokio::runtime::Runtime;

#[derive(clap::Args)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
pub struct Pass {
    #[command(subcommand)]
    pub command: Option<PassCommand>,
    /// The pass configuration (docs/pass.md), from a trusted default-branch checkout [default: .github/pass.json in the current directory]. Credentials come from GH_TOKEN, GITHUB_TOKEN or gh auth token (in that order), never the file; DISPATCH_TOKEN overrides the token for starts.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// The caller's agentic-job bounds file for its `fix` kind (such as .github/agentic-job/fix.toml), from the same trusted checkout, never a dispatched head or workflow input: a pull request whose repository is outside its `repos` (unless push_to_pull_request_branch's `own_author` is the pull request's author), or whose head branch is outside its push_to_pull_request_branch `branches`, gets no fix, rebase or continuation round and is left to the operator. Repeatable: a pull request is inside where any one file admits both its repository and its head branch. The policy step stays the boundary.
    #[arg(long, value_name = "FILE")]
    pub fix_bounds: Vec<PathBuf>,
    /// Take the steps: follow up pull requests, publish the usage snapshot, apply epic plans, start the chosen runs, write board Status and, with holds, publish the hold snapshot as GH_TOKEN's bot, starting runs with DISPATCH_TOKEN if set. Without it the pass previews and writes nothing. In a GitHub Actions run (GITHUB_REPOSITORY, GITHUB_RUN_ID), work starts only while no other run of its workflow is in progress. Outside one, it needs both dispatch and write caps (config or flags), and is not serialized against the scheduled workflow.
    #[arg(long)]
    pub apply: bool,
    /// Start at most N agent runs in this pass: pull request fix, rebase and continuation rounds, an epic's planning run, and scheduled starts. The rest are held, each line saying so. Overrides config maxDispatch.
    #[arg(long, value_name = "N")]
    pub max_dispatch: Option<usize>,
    /// Take at most N other write steps in this pass: forge writes such as comments, labels, merges, the usage and hold snapshots, epic plans and Status (a pull request's comment, label, retitle, retarget, enqueue or merge step, the usage and hold comments' edits, an epic plan's receipt and each of its actions that writes, and each item's Status change). A step is taken whole or held whole, and an epic plan's steps together. A pass that made progress publishes its hold snapshot one past a full cap above 0, so its progress is kept. Overrides config maxWrites.
    #[arg(long, value_name = "N")]
    pub max_writes: Option<usize>,
    /// Refuse the board Status stage whole when it would change more than N items' Status [default: config maxStatusChanges, otherwise 50]: the first pass over a board that drifted is looked at first. Pass a higher N once to let it through.
    #[arg(long, value_name = "N")]
    pub max_status_changes: Option<usize>,
    /// Preview a recorded pass (a replay fixture) at its clock instead of reading the forge; needs no token. A Forgejo fixture replays only on the instance it was recorded on. On GitHub, with --record, it seeds the recording instead: its answers stand in for live reads, such as a usage snapshot the token cannot read or the broker's /usage.
    #[arg(long, value_name = "FILE", conflicts_with = "apply")]
    pub replay: Option<PathBuf>,
    /// Preview live, and save every read and its answer to FILE as a replay fixture.
    #[arg(long, value_name = "FILE", conflicts_with = "apply")]
    pub record: Option<PathBuf>,
    /// Only check --config and --fix-bounds as a pass would, and exit. Explicit repository lists need no token or forge reads; organization entries need GH_TOKEN to resolve their bounded listing before policy validation. For a caller's pull request CI, so a configuration the pass refuses fails review instead of every pass after merge.
    #[arg(long, conflicts_with_all = ["apply", "replay", "record"])]
    pub check_config: bool,
}

/// What `pass --check-config` prints for a configuration the pass accepts.
const CONFIG_OK: &str = "pass: configuration ok";

#[derive(clap::Subcommand)]
pub enum PassCommand {
    /// After an applying pass in GitHub Actions (GITHUB_REPOSITORY, GITHUB_RUN_ID): wait until --wait-minutes after this run started, wake optional stale auxiliary workflows, then dispatch --workflow unless a newer applying run of it on --branch is still to finish; GH_TOKEN needs actions: write (docs/pass.md#the-next-pass).
    Next(Next),
}

#[derive(clap::Args)]
pub struct Next {
    /// The pass workflow to dispatch, such as pass.yml; its runs are the ones checked for a newer applying run.
    #[arg(long, value_name = "FILE")]
    pub workflow: String,
    /// The default branch: the ref dispatched, and the branch whose runs are read.
    #[arg(long)]
    pub branch: String,
    /// An input of the pass dispatch, such as apply=true. Repeatable.
    #[arg(long, value_name = "NAME=VALUE", value_parser = input)]
    pub input: Vec<(String, String)>,
    /// The run title of a dispatched pass that applies; a scheduled run always counts as applying.
    #[arg(long, value_name = "TITLE")]
    pub apply_title: String,
    /// The pass interval: start the next pass this many minutes after this run started, never waiting longer.
    #[arg(long, value_name = "MINUTES")]
    pub wait_minutes: u64,
    /// A supervisor workflow to start, before the pass, when its newest run on --branch, ignoring issue_comment runs, is at least --supervisor-stale-minutes old or is not among its newest 100. Its failures only warn.
    #[arg(long, value_name = "FILE", requires = "supervisor_stale_minutes")]
    pub supervisor_workflow: Option<String>,
    /// How old the supervisor's newest run may be before it is started.
    #[arg(long, value_name = "MINUTES", requires = "supervisor_workflow")]
    pub supervisor_stale_minutes: Option<u64>,
    /// An input of the supervisor dispatch, such as session=false. Repeatable.
    #[arg(long, value_name = "NAME=VALUE", value_parser = input, requires = "supervisor_workflow")]
    pub supervisor_input: Vec<(String, String)>,
    /// A release-train workflow to start before the pass when its newest run on --branch is at least --train-stale-minutes old or is not among its newest 100. All run events count. Its failures only warn. Off unless supplied.
    #[arg(long, value_name = "FILE", requires = "train_stale_minutes")]
    pub train_workflow: Option<String>,
    /// How old the train's newest run may be before it is started; independent of the supervisor.
    #[arg(long, value_name = "MINUTES", requires = "train_workflow")]
    pub train_stale_minutes: Option<u64>,
    /// An input of the train dispatch. Repeatable.
    #[arg(long, value_name = "NAME=VALUE", value_parser = input, requires = "train_workflow")]
    pub train_input: Vec<(String, String)>,
    /// Print what would wait and start; wait for and start nothing.
    #[arg(long)]
    pub dry_run: bool,
}

fn input(text: &str) -> Result<(String, String)> {
    let (name, value) = text
        .split_once('=')
        .with_context(|| format!("{text:?} is not NAME=VALUE"))?;
    ensure!(!name.is_empty(), "{text:?} has no input name");
    Ok((name.into(), value.into()))
}

fn next(args: &Next, out: &mut impl Write) -> Result<()> {
    use board_forge::pass::next::{self, Supervisor, Workflow};
    let var = |name| {
        std::env::var(name)
            .with_context(|| format!("pass next runs in a GitHub Actions run, which sets {name}"))
    };
    let workflow = |file: &str, inputs: &[(String, String)]| {
        let mut workflow = Workflow {
            file: file.into(),
            inputs: Default::default(),
        };
        for (name, value) in inputs {
            let first = workflow.inputs.insert(name.clone(), value.clone());
            ensure!(first.is_none(), "input {name} of {file} given twice");
        }
        Ok(workflow)
    };
    let minutes = |m: u64| Duration::from_secs(m.saturating_mul(60));
    let supervisor = match (&args.supervisor_workflow, args.supervisor_stale_minutes) {
        (Some(file), Some(stale)) => Some(Supervisor {
            workflow: workflow(file, &args.supervisor_input)?,
            stale: minutes(stale),
        }),
        _ => None,
    };
    let settings = next::Settings {
        repository: var("GITHUB_REPOSITORY")?,
        run: var("GITHUB_RUN_ID")?.parse().context("GITHUB_RUN_ID")?,
        branch: args.branch.clone(),
        pass: workflow(&args.workflow, &args.input)?,
        apply_title: args.apply_title.clone(),
        interval: minutes(args.wait_minutes),
        supervisor,
        train: match (&args.train_workflow, args.train_stale_minutes) {
            (Some(file), Some(stale)) => Some(Supervisor {
                workflow: workflow(file, &args.train_input)?,
                stale: minutes(stale),
            }),
            _ => None,
        },
        dry_run: args.dry_run,
    };
    settings.validate()?;
    // A deadline already past: the client waits out no rate limit itself, so
    // `pass next`'s own four tries bound each call, as GitHub's job timeout
    // would otherwise end it mid-wait.
    let client = Client::from_env()?.retry_within(Instant::now());
    next::run(
        &client,
        &next::SystemClock,
        &settings,
        out,
        &mut std::io::stderr().lock(),
    )
}

/// A preview through `live` whose reads are saved to `path` as a replay fixture.
fn record_pass<C: Rest + Transport + GraphQl + pass::Broker>(
    runtime: &Runtime,
    live: &C,
    config: &Config,
    (path, limits): (&PathBuf, Limits),
    now: time::OffsetDateTime,
) -> Result<Report> {
    let recorder = Recorder::new(live);
    let preview = (Mode::Preview, limits);
    let pass = pass::run(&recorder, &recorder, config, State::default(), preview, now);
    let report = runtime.block_on(pass)?;
    std::fs::write(path, recorder.fixture(now)?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(report)
}

/// Set to `true` in every GitHub Actions run.
const ACTIONS_VAR: &str = "GITHUB_ACTIONS";

/// How an applying pass is kept from racing the scheduled one: in GitHub
/// Actions by its run ([`own_run`]); outside, only by its caps, so both must
/// be given in config or flags. Checked before any token is read.
fn apply_mode(limits: Limits) -> Result<Option<PassRun>> {
    if std::env::var(ACTIONS_VAR).as_deref() == Ok("true") {
        return own_run().map(Some);
    }
    if limits.dispatch.is_none() || limits.writes.is_none() {
        bail!(
            "--apply outside GitHub Actions needs both --max-dispatch and --max-writes (or config maxDispatch and maxWrites): a local pass is not serialized against the scheduled workflow, so its caps bound what it can do"
        );
    }
    Ok(None)
}

/// The GitHub Actions run this applying pass is in ([`PassRun`]).
fn own_run() -> Result<PassRun> {
    let var = |name| {
        std::env::var(name).with_context(|| {
            format!(
                "--apply runs in a GitHub Actions run, which sets {name}, so that two passes never start work at once"
            )
        })
    };
    let repository = var("GITHUB_REPOSITORY")?;
    board_forge::poll::validate_repository(&repository).context("GITHUB_REPOSITORY")?;
    let id = var("GITHUB_RUN_ID")?.parse().context("GITHUB_RUN_ID")?;
    Ok(PassRun { repository, id })
}

/// Forgejo exports its own Actions variables and GitHub-compatible aliases.
/// Check the server before reading credentials: an environment from another
/// instance must not serialize against the wrong pass workflow.
fn forgejo_apply_mode(
    board: &board_forge::forgejo::Config,
    limits: Limits,
) -> Result<Option<PassRun>> {
    let var = |key: &str| {
        std::env::var(format!("FORGEJO_{key}")).or_else(|_| std::env::var(format!("GITHUB_{key}")))
    };
    if var("ACTIONS").as_deref() == Ok("true") {
        let server = var("SERVER_URL").context("Forgejo Actions needs SERVER_URL")?;
        ensure!(
            server.trim_end_matches('/') == board.url.as_str().trim_end_matches('/'),
            "Actions server URL differs from configured board.url"
        );
        let repository = var("REPOSITORY").context("Forgejo Actions needs REPOSITORY")?;
        board_forge::forgejo::validate_repository(&repository)?;
        let id = var("RUN_ID")
            .context("Forgejo Actions needs RUN_ID")?
            .parse()
            .context("Forgejo RUN_ID")?;
        ensure!(id > 0, "invalid Forgejo RUN_ID");
        return Ok(Some(PassRun { repository, id }));
    }
    ensure!(
        limits.dispatch.is_some() && limits.writes.is_some(),
        "--apply outside Forgejo Actions needs both --max-dispatch and --max-writes (or config maxDispatch and maxWrites)"
    );
    Ok(None)
}

fn read(path: &PathBuf) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}

/// One pass: a line per decision on stdout, after one saying whether it
/// previews or applies, and a summary of its caps last. Any failed operation
/// fails the command, after the rest of the pass has run.
pub fn run(args: &Pass, runtime: &Runtime, out: &mut impl Write) -> Result<()> {
    if let Some(PassCommand::Next(args)) = &args.command {
        return next(args, out);
    }
    let default_path = PathBuf::from(".github/pass.json");
    let path = args.config.as_ref().unwrap_or(&default_path);
    let mut config =
        Config::parse_sources(&read(path)?).with_context(|| format!("{}", path.display()))?;
    let limits = Limits {
        dispatch: args.max_dispatch.or(config.max_dispatch),
        writes: args.max_writes.or(config.max_writes),
        status_changes: args.max_status_changes.or(config.max_status_changes),
    };
    let bounds = if args.fix_bounds.is_empty() {
        let directory = path.parent().context("config has no parent directory")?;
        config
            .fix_bounds_files
            .iter()
            .map(|file| directory.join(file))
            .collect::<Vec<_>>()
    } else {
        args.fix_bounds.clone()
    };
    if let Some(board) = config.forgejo_board() {
        // Stop unsupported operations before opening files or looking up any
        // credentials, including the GitHub apply/record/replay paths below.
        for (supplied, option, reason) in [
            (
                !bounds.is_empty(),
                "--fix-bounds",
                "PR follow-up has no reviewed Forgejo equivalent",
            ),
            (
                args.record.is_some() && args.replay.is_some(),
                "--record with --replay",
                "a seeded recording is GitHub-only",
            ),
        ] {
            ensure!(!supplied, "unsupported_capability: {option}: {reason}");
        }
        if args.check_config {
            writeln!(out, "{CONFIG_OK}")?;
            return Ok(());
        }
        if let Some(path) = &args.replay {
            // No client, broker or credential variable: only the fixture.
            let value = serde_json::from_str(&read(path)?)
                .with_context(|| format!("decode {}", path.display()))?;
            let replay = pass::forgejo::Replay::from_json(&value, board)
                .with_context(|| format!("{}", path.display()))?;
            writeln!(out, "pass: preview")?;
            let preview = (Mode::Preview, limits);
            let report = pass::forgejo::run(&replay, &replay, &config, preview, replay.now)?;
            let finished = replay.finish();
            print_report(&report, out)?;
            return finished.with_context(|| format!("{} replayed another pass", path.display()));
        }
        let own = args
            .apply
            .then(|| forgejo_apply_mode(board, limits))
            .transpose()?;
        let mode = match &own {
            None => Mode::Preview,
            Some(Some(own)) => Mode::Actions(own),
            Some(None) => Mode::Local,
        };
        if let Some(warning) = board.plain_http_warning() {
            eprintln!("{warning}");
        }
        let client = board_forge::forgejo::Client::from_env(board)?;
        let broker = pass::HttpBroker::default();
        writeln!(
            out,
            "pass: {}",
            if args.apply { "apply" } else { "preview" }
        )?;
        if matches!(mode, Mode::Local) {
            writeln!(
                out,
                "warning: applying outside Forgejo Actions, not serialized against the scheduled workflow's passes"
            )?;
        }
        let now = time::OffsetDateTime::now_utc();
        let report = match &args.record {
            // A preview: --record conflicts with --apply.
            Some(path) => {
                let recorder = pass::forgejo::Recorder::new(&client, &broker, board);
                let report =
                    pass::forgejo::run(&recorder, &recorder, &config, (mode, limits), now)?;
                std::fs::write(path, recorder.fixture(now)?)
                    .with_context(|| format!("write {}", path.display()))?;
                report
            }
            None => pass::forgejo::run(&client, &broker, &config, (mode, limits), now)?,
        };
        return print_report(&report, out);
    }
    config.fix_bounds = FixBoundsSet::read(&bounds)?;
    if args.check_config {
        if config.has_repository_sources() {
            let client = pass_client()?.retry_within(
                Instant::now()
                    + Duration::from_secs(board_forge::repository_lists::RESOLUTION_SECONDS),
            );
            let mut lines = Vec::new();
            config.resolve(&client, &mut lines)?;
            for line in lines {
                eprintln!("{line}");
            }
        }
        writeln!(out, "{CONFIG_OK}")?;
        return Ok(());
    }
    let own = args.apply.then(|| apply_mode(limits)).transpose()?;
    writeln!(
        out,
        "pass: {}",
        if args.apply { "apply" } else { "preview" }
    )?;
    if let Some(None) = own {
        writeln!(
            out,
            "warning: applying outside GitHub Actions, not serialized against the scheduled workflow's passes"
        )?;
    }
    let replay = match &args.replay {
        Some(path) => {
            let value = serde_json::from_str(&read(path)?)
                .with_context(|| format!("decode {}", path.display()))?;
            Some(Replay::from_json(&value)?)
        }
        None => None,
    };
    let report = match (&replay, &args.record) {
        (Some(replay), None) => {
            let state = State::default();
            let preview = (Mode::Preview, limits);
            runtime.block_on(pass::run(
                replay, replay, &config, state, preview, replay.now,
            ))?
        }
        (seed, record) => {
            let now = time::OffsetDateTime::now_utc();
            // No wait under a rate limit outlasts the pass's time bound,
            // which starts with it.
            let deadline = Instant::now() + Duration::from_secs(pass::PASS_SECONDS_LIMIT);
            let client = pass_client()?.retry_within(deadline);
            match (seed, record) {
                (Some(seed), Some(path)) => {
                    let seeded = Seeded {
                        seed,
                        live: &client,
                    };
                    record_pass(runtime, &seeded, &config, (path, limits), now)?
                }
                (None, Some(path)) => record_pass(runtime, &client, &config, (path, limits), now)?,
                (_, None) => {
                    let lost = config.stateless_losses();
                    if !lost.is_empty() {
                        eprintln!(
                            "warning: no state in {}, so this pass keeps nothing for the next: {}",
                            path.display(),
                            lost.join("; ")
                        );
                    }
                    // Only an applying pass writes the cache; a preview reads it.
                    let state = config.state.as_deref().map(load_state).unwrap_or_default();
                    let mode = match &own {
                        None => Mode::Preview,
                        Some(Some(own)) => Mode::Actions(own),
                        Some(None) => Mode::Local,
                    };
                    let dispatcher = Client::from_var("DISPATCH_TOKEN")
                        .map(|dispatcher| dispatcher.retry_within(deadline));
                    let dispatcher = dispatcher.as_ref().unwrap_or(&client);
                    let pass = pass::run(&client, dispatcher, &config, state, (mode, limits), now);
                    let report = runtime.block_on(pass)?;
                    if let (true, Some(path), Some(state)) =
                        (args.apply, &config.state, &report.state)
                    {
                        std::fs::write(path, serde_json::to_vec(&state.to_json())?)
                            .with_context(|| format!("write {path}"))?;
                    }
                    report
                }
            }
        }
    };
    print_report(&report, out)
}

/// The pass's client, and `changes`'s (docs/pass.md#local-github-credentials).
pub(crate) fn pass_client() -> Result<Client> {
    Client::from_env_or_else(|| {
        let mut command = std::process::Command::new("gh");
        // This client talks to api.github.com, even when gh's default host
        // (GH_HOST) is an enterprise instance. Never borrow that host's token.
        command.args(["auth", "token", "--hostname", "github.com"]);
        gh_token(command)
            .map_err(|_| anyhow::Error::from(board_forge::github::AuthenticationRequired))
            .context("pass needs GH_TOKEN, GITHUB_TOKEN or a working gh auth login for github.com")
    })
}

/// Do not forward gh's stdout/stderr, including on failure: either may contain
/// credentials. This is a local credential lookup, not a forge request.
fn gh_token(mut command: std::process::Command) -> Result<String> {
    use std::process::Stdio;

    let output = command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("run gh auth token")?;
    ensure!(output.status.success(), "gh auth token failed");
    let token = String::from_utf8(output.stdout).context("gh token is not UTF-8")?;
    let token = token.trim();
    ensure!(
        !token.is_empty() && !token.chars().any(char::is_whitespace),
        "gh token is empty or contains whitespace"
    );
    Ok(token.to_owned())
}

/// This file is supplied only by the trusted previous pass in the chain.
/// Missing, oversized or unreadable state costs requests, not availability.
fn load_state(path: &str) -> State {
    let load = || -> Result<State> {
        use std::io::Read;
        let file = std::fs::File::open(path)?;
        let mut bytes = Vec::new();
        file.take(pass::STATE_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= pass::STATE_BYTES,
            "pass state is too large"
        );
        State::from_json(&serde_json::from_slice(&bytes)?)
    };
    match load() {
        Ok(state) => state,
        Err(_) => {
            eprintln!("warning: missing or unreadable pass state; starting cold");
            State::default()
        }
    }
}

fn print_report(report: &Report, out: &mut impl Write) -> Result<()> {
    for line in &report.lines {
        writeln!(out, "{line}")?;
    }
    writeln!(out, "{}", report.summary_line())?;
    ensure!(
        report.failures == 0,
        "{} operation(s) of the pass failed",
        report.failures
    );
    Ok(())
}

#[cfg(test)]
mod next_tests {
    use super::*;
    use clap::Parser;

    #[cfg(unix)]
    #[test]
    fn gh_token_is_trimmed_and_failures_do_not_reveal_output() {
        for (script, expected) in [
            ("printf 'local-login-token\\n'", Some("local-login-token")),
            ("exit 0", None),
            ("printf 'two tokens'", None),
            ("printf '\\377'", None),
            (
                "printf 'private-output'; printf 'private-error' >&2; exit 1",
                None,
            ),
        ] {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", script]);
            let result = gh_token(command);
            match expected {
                Some(token) => assert_eq!(result.unwrap(), token),
                None => {
                    let error = format!("{:#}", result.unwrap_err());
                    assert!(!error.contains("private-output"));
                    assert!(!error.contains("private-error"));
                    assert!(!error.contains("two tokens"));
                }
            }
        }
        assert!(gh_token(std::process::Command::new("/nonexistent/agent-board-gh")).is_err());
    }

    #[test]
    fn pass_state_is_optional_bounded_and_cold_on_decode_failure() {
        let path = std::env::temp_dir().join(format!(
            "agent-board-pass-state-{}.json",
            std::process::id()
        ));
        let name = path.to_str().unwrap();
        assert_eq!(load_state(name), State::default());
        std::fs::write(&path, b"not JSON").unwrap();
        assert_eq!(load_state(name), State::default());
        let state = State {
            interval_seconds: board_forge::poll::MIN_INTERVAL_SECONDS,
            ..State::default()
        };
        std::fs::write(&path, serde_json::to_vec(&state.to_json()).unwrap()).unwrap();
        assert_eq!(load_state(name), state);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(pass::STATE_BYTES + 1)
            .unwrap();
        assert_eq!(load_state(name), State::default());
        std::fs::remove_file(path).unwrap();
    }

    #[derive(Parser)]
    struct Args {
        #[command(flatten)]
        next: Next,
    }

    #[test]
    fn next_flag_decoding() {
        let base = [
            "next",
            "--workflow",
            "pass.yml",
            "--branch",
            "main",
            "--apply-title",
            "pass (apply)",
            "--wait-minutes",
            "10",
        ];
        let decode = |extra: &[&str]| {
            Args::try_parse_from(base.iter().copied().chain(extra.iter().copied()))
        };
        let omitted = decode(&[]).unwrap().next;
        assert!(omitted.train_workflow.is_none());
        assert!(omitted.train_stale_minutes.is_none());
        assert!(omitted.train_input.is_empty());
        let both = decode(&[
            "--supervisor-workflow",
            "supervisor.yml",
            "--supervisor-stale-minutes",
            "30",
            "--supervisor-input",
            "session=false",
            "--train-workflow",
            "train.yml",
            "--train-stale-minutes",
            "15",
            "--train-input",
            "release=true",
            "--train-input",
            "prompt=",
        ])
        .unwrap()
        .next;
        assert_eq!(both.supervisor_stale_minutes, Some(30));
        assert_eq!(both.train_stale_minutes, Some(15));
        assert_eq!(
            both.train_input,
            [
                ("release".into(), "true".into()),
                ("prompt".into(), "".into())
            ]
        );
        for extra in [
            vec!["--train-workflow", "train.yml"],
            vec!["--train-stale-minutes", "15"],
            vec!["--train-input", "release=true"],
            vec![
                "--train-workflow",
                "train.yml",
                "--train-stale-minutes",
                "-1",
            ],
            vec![
                "--train-workflow",
                "train.yml",
                "--train-stale-minutes",
                "abc",
            ],
            vec![
                "--train-workflow",
                "train.yml",
                "--train-stale-minutes",
                "15",
                "--train-input",
                "release",
            ],
            vec![
                "--train-workflow",
                "train.yml",
                "--train-stale-minutes",
                "15",
                "--train-input",
                "=true",
            ],
        ] {
            assert!(decode(&extra).is_err(), "{extra:?}");
        }
    }
}
