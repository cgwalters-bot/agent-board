mod action;
mod apps;
mod ask;
mod broker;
mod budget;
mod cli_docs;
mod decisions;
mod dispatch;
mod epic;
mod item;
mod live;
mod org_status;
mod pass;
mod poll;
mod prune;
mod release_train;
mod review;
mod supervise;

use anyhow::{Context, Result, bail, ensure};
use board_core::{Capability, Policy, Snapshot};
use board_forge::{GitHubSafeOutputs, OutputBackend, parse_github_item};
use clap::{Args, Parser, Subcommand};
use jaq_interpret::FilterT;
use serde_json::Value;
use std::{
    io::{self, Write},
    path::PathBuf,
};
use tokio::runtime::Runtime;

#[derive(Parser)]
#[command(
    name = "agent-board",
    version,
    about = "Typed board snapshots and preview-only proposals"
)]
struct Cli {
    /// Trusted identity policy; defaults to the policy compiled from this repository.
    #[arg(long, global = true)]
    policy: Option<PathBuf>,
    /// Read an offline board snapshot instead of GitHub.
    #[arg(long, global = true)]
    snapshot: Option<PathBuf>,
    /// Print selected comma-separated JSON fields.
    #[arg(long, global = true, value_name = "FIELDS")]
    json: Option<String>,
    /// Filter JSON output with a jq expression.
    #[arg(long, global = true, requires = "json")]
    jq: Option<String>,
    /// Repository OWNER/REPO for filtering or bare item numbers.
    #[arg(short = 'R', long, global = true)]
    repo: Option<String>,
    /// GitHub hostname (only github.com is supported for live reads).
    #[arg(long, global = true, default_value = board_forge::github::DEFAULT_HOST)]
    hostname: String,
    /// Organization or user owning the live project.
    #[arg(long, global = true, conflicts_with = "snapshot")]
    owner: Option<String>,
    /// Live project number, not its node ID.
    #[arg(long, global = true, conflicts_with = "snapshot")]
    project: Option<u64>,
    /// Trusted default-branch agent-board.json whose `board` names the forge (GitHub by default).
    #[arg(long, global = true, value_name = "FILE", conflicts_with = "snapshot")]
    board_config: Option<PathBuf>,
    #[arg(long, global = true, hide = true)]
    template: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate proposed operator mutations; never execute them.
    Action {
        #[command(subcommand)]
        command: Action,
    },
    /// Check proposals without applying them.
    Check {
        #[command(subcommand)]
        command: Check,
    },
    /// Inspect a project's summary and schema.
    Project {
        #[command(subcommand)]
        command: Project,
    },
    /// What landed, closed, opened or changed hold on the live project since a time, each line a link (read-only); --board-config's operator counts as an author whose new pull requests are listed.
    Changes {
        /// Start of the window, RFC 3339 (such as 2026-10-23T00:00:00Z).
        #[arg(long)]
        since: String,
        /// End of the window, RFC 3339; without it, the window runs to now.
        #[arg(long)]
        until: Option<String>,
        /// Output format.
        #[arg(long, default_value = "markdown", value_parser = ["markdown", "json"])]
        format: String,
        /// Leave out issues and pull requests of private repositories, for a
        /// list that will be published.
        #[arg(long)]
        public_only: bool,
        /// Trim the Markdown to at most N characters as a generated status
        /// update is trimmed: the longest sections lose their last lines
        /// first, each ending with `+K more`.
        #[arg(long, value_name = "N")]
        max_chars: Option<usize>,
    },
    /// List or inspect board items.
    Item {
        #[command(subcommand)]
        command: Item,
    },
    /// Ask the operator a question as an issue, list or resolve asks.
    Ask {
        #[command(subcommand)]
        command: Ask,
    },
    /// Propose run field updates, or write them with --apply.
    Run {
        #[command(subcommand)]
        command: Run,
    },
    /// Prepare an operator-approved workflow dispatch (no network writes).
    Request {
        #[command(subcommand)]
        command: Request,
    },
    /// Start the job an issue's newest accepted plan comment proposes, once (--dry-run prints the request).
    Dispatch(dispatch::FromPlan),
    /// Check a coordinator's plan for an epic's sub-issues.
    Epic {
        #[command(subcommand)]
        command: Epic,
    },
    /// Send an operator-approved review draft as the bot.
    Review {
        #[command(subcommand)]
        command: Review,
    },
    /// Save a normalized board snapshot for offline use.
    Snapshot {
        #[command(subcommand)]
        command: SnapshotCommand,
    },
    /// Preview deterministic board repairs; never apply them.
    Reconcile {
        #[command(subcommand)]
        command: Reconcile,
    },
    #[command(hide = true)]
    Output {
        #[command(subcommand)]
        command: Output,
    },
    /// One pass of the pull request poll, previewed or applied; says when the next is due.
    Poll(poll::Poll),
    /// One scheduled pass: follow up pull requests, apply and plan live epics, then choose and start board work by Priority, WIP and pace; previews unless --apply (docs/pass.md).
    Pass(pass::Pass),
    /// What each item under the board's open epics has cost, sub-issues included, beside the board's weekly budget and each item's Budget ($) field, or with --split a window's spend split between upstream work and the harness (read-only; docs/pass.md#budget-tree).
    Budget(budget::Budget),
    /// Open issues of the board's work repositories that are candidates to close (overtaken, duplicate, stale or triaged done), each with its reasons and evidence, unless labelled keep (read-only; docs/prune.md).
    Prune(prune::Prune),
    /// The controller's supervisor: a read-only health pass, whether its agent runs, and the executor of its dispatch requests.
    Supervise(supervise::Supervise),
    /// How loaded the --owner organization is: runner jobs, agent runs, pools, merge queues and, with --project, the board (read-only; GH_TOKEN reads).
    Status(org_status::Status),
    /// One pass of the release train: cut a release and move its callers' pins, a step a pass (prints unless --apply).
    ReleaseTrain(release_train::ReleaseTrain),
    /// Probe the inference broker and keep one outage issue open while it is unreachable.
    Broker {
        #[command(subcommand)]
        command: Broker,
    },
    /// Create the per-role GitHub Apps (docs/apps.md).
    Apps {
        #[command(subcommand)]
        command: Apps,
    },
    /// Run the credential-bearing staging integration test.
    Test {
        #[command(subcommand)]
        command: Test,
    },
    /// Print this CLI's reference as Markdown (docs/cli.md).
    #[command(hide = true)]
    HelpMarkdown,
}

#[derive(Subcommand)]
enum Broker {
    /// Probe the broker (TCP, then one HTTP request) and open, refresh or close the outage issue; prints unless --apply, and fails while the broker is unreachable.
    Probe(broker::Probe),
}

#[derive(Subcommand)]
enum Epic {
    /// Print the mutations the newest accepted epic plan on an epic would make (--dry-run only).
    Plan(epic::Plan),
    /// Apply the newest accepted epic plan once (dry run is the default; --apply writes).
    Apply(epic::Apply),
}

#[derive(Subcommand)]
enum Review {
    /// Post the newest review draft on an issue once the operator approves it (dry run unless --apply).
    Send(review::Send),
}

#[derive(Subcommand)]
enum Action {
    /// Print the newest accepted proposal's typed mutations without writing.
    Check(action::Check),
}

#[derive(Subcommand)]
enum Project {
    /// Show item counts and available fields (use --json for snapshot data).
    View,
    /// Create the Turn, Run and Result fields dispatch needs, if missing (writes).
    Init,
    /// Publish a project status update (writes; requires project write permission).
    StatusUpdate {
        /// Overall project health.
        #[arg(long, value_parser = ["on-track", "at-risk", "off-track"])]
        status: String,
        /// Read the update body from a UTF-8 file; otherwise post the changes since the last update.
        #[arg(long, value_name = "FILE")]
        body: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum Item {
    /// List board items, including closed issues by default.
    List(List),
    /// Inspect an item by URL, OWNER/REPO#N, or N with --repo.
    View {
        /// Item URL, OWNER/REPO#N, or N with --repo.
        item: String,
    },
    /// Find open board issues of --config's workRepositories by title or body text (read-only).
    Search(item::Search),
    /// Add an issue to the board, optionally setting Priority (writes with --apply).
    Add(item::Add),
    #[command(hide = true)]
    Create,
    /// Set single-select or text fields on a board issue (writes with --apply).
    Edit(item::Edit),
    #[command(hide = true)]
    Close { item: String },
}

#[derive(Args)]
struct List {
    /// Filter by exact board status (for example "In Progress").
    #[arg(long)]
    status: Option<String>,
    /// Filter by priority: P0 to P3.
    #[arg(long)]
    priority: Option<String>,
    /// Filter by whose turn it is (for example Coordinator).
    #[arg(long)]
    turn: Option<String>,
    /// Filter by issue state.
    #[arg(long, default_value = "all", value_parser = ["open", "closed", "all"])]
    state: String,
    /// Maximum number of items to display.
    #[arg(short = 'L', long, default_value_t = 30)]
    limit: usize,
    /// List the open issues of --config's workRepositories that are not on its board instead, without filters.
    #[arg(long, conflicts_with_all = ["status", "priority", "turn", "state"])]
    off_board: bool,
    #[command(flatten)]
    read: item::Read,
}

#[derive(Subcommand)]
enum Ask {
    /// Append to the configured decisions document (preview unless --apply).
    Add(decisions::Add),
    /// Report edits since the bot's last write to the decisions document.
    Read(decisions::Settings),
    /// Open a question issue and label it and the blocked item needs-human (writes with --apply).
    Create(ask::Create),
    /// List open board issues labelled question.
    List {
        /// Maximum number of asks to display.
        #[arg(short = 'L', long, default_value_t = 30)]
        limit: usize,
    },
    /// Comment the answer, close the ask and unlabel the item it blocked (writes with --apply).
    Resolve(ask::Resolve),
}

#[derive(Subcommand)]
enum Run {
    #[command(hide = true)]
    List,
    #[command(hide = true)]
    View { run: String },
    /// Propose recording an operator-verified run and optional applied result.
    Record(dispatch::Record),
}

#[derive(Subcommand)]
enum Request {
    #[command(hide = true)]
    Create {
        #[arg(long)]
        item: String,
    },
    #[command(hide = true)]
    View { issue: String },
    /// Print gh argv for one eligible issue; does not execute it.
    Dispatch(dispatch::Dispatch),
}

#[derive(Subcommand)]
enum SnapshotCommand {
    /// Write the complete normalized snapshot as JSON to stdout.
    Create,
}

#[derive(Subcommand)]
enum Reconcile {
    /// Propose repairs from a live board or offline snapshot.
    Plan {
        /// Write safe-output proposals as JSONL, not authorized mutations.
        #[arg(long)]
        emit: bool,
        /// Trusted default-branch agent-board.json; statusReconcile is off by default.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum Output {
    Check,
    Apply,
}

#[derive(Subcommand)]
enum Check {
    /// Refuse outputs outside an identity's assignment and field policy.
    Proposals {
        /// Identity from the policy whose bounds the proposals must stay within.
        #[arg(long = "as")]
        identity: String,
        /// JSONL proposals file; reads stdin when omitted.
        #[arg(long, value_name = "FILE")]
        proposals: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum Apps {
    /// Register a role's GitHub App from the roles config through GitHub's manifest flow (one browser click).
    Create(apps::Create),
}

#[derive(Subcommand)]
enum Test {
    /// Create and clean up scratch objects in the staging project (writes).
    Project(live::Options),
}

fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = if error.use_stderr() { 1 } else { 0 };
            let _ = error.print();
            std::process::exit(code);
        }
    };
    // One runtime for the process; only the GitHub snapshot is async so far.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("start the async runtime");
    if let Err(error) = runtime.and_then(|runtime| execute(cli, &runtime)) {
        eprintln!("error: {error:#}");
        std::process::exit(live::error_exit_code(&error));
    }
}

fn agent_policy(cli: &Cli) -> Result<board_core::AgentPolicy> {
    let input = match &cli.policy {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("read policy {}", path.display()))?,
        None => board_core::DEFAULT_AGENT_POLICY.to_owned(),
    };
    board_core::AgentPolicy::parse(&input)
}

/// The `operator` of `--board-config`, if one is given.
fn configured_operator(cli: &Cli) -> Result<Option<String>> {
    let Some(path) = &cli.board_config else {
        return Ok(None);
    };
    let value: Value = serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    Ok(value["operator"].as_str().map(str::to_owned))
}

fn github(cli: &Cli) -> Result<board_forge::github::GitHub<board_forge::github::Client>> {
    ensure!(
        cli.hostname == board_forge::github::DEFAULT_HOST,
        "unsupported GitHub host"
    );
    Ok(board_forge::github::GitHub {
        owner: cli.owner.clone().context("live commands require --owner")?,
        number: cli.project.context("live commands require --project")?,
        transport: board_forge::github::Client::from_env()?,
    })
}

/// The board `--board-config` selects. Only the read commands accept a
/// Forgejo one: it has no write path, and writes elsewhere assume GitHub.
fn board(cli: &Cli) -> Result<board_forge::forgejo::Board> {
    let Some(path) = &cli.board_config else {
        return Ok(board_forge::forgejo::Board::Github {});
    };
    let board = board_forge::forgejo::Board::from_config(&supervise::read(path)?)?;
    ensure!(
        matches!(board, board_forge::forgejo::Board::Github {})
            || matches!(
                cli.command,
                Command::Snapshot { .. }
                    | Command::Project {
                        command: Project::View
                    }
                    | Command::Item {
                        command: Item::List(_) | Item::View { .. }
                    }
                    | Command::Reconcile { .. }
            ),
        "unsupported_capability: a Forgejo board is only read by snapshot create, project view, item list, item view and reconcile plan"
    );
    Ok(board)
}

fn load(cli: &Cli, runtime: &Runtime) -> Result<Snapshot> {
    let policy = agent_policy(cli)?;
    let live = if let board_forge::forgejo::Board::Forgejo(config) = board(cli)? {
        ensure!(
            cli.owner.is_none() && cli.project.is_none(),
            "a Forgejo board is chosen by its config, not --owner or --project"
        );
        let transport = board_forge::forgejo::Client::from_env(&config)?;
        Some(board_forge::forgejo::Forgejo { transport, config }.snapshot_with_policy(&policy)?)
    } else if cli.snapshot.is_none() {
        let github = github(cli)?;
        let budget = board_forge::github::SNAPSHOT_REQUEST_LIMIT;
        Some(runtime.block_on(github.snapshot_with_policy(&policy, budget))?)
    } else {
        None
    };
    if let Some(snapshot) = live {
        if !snapshot.coverage.complete {
            eprintln!("Warning: snapshot coverage is incomplete; reconciliation is blocked.");
        }
        return Ok(snapshot);
    }
    let path = cli
        .snapshot
        .as_ref()
        .context("--snapshot FILE is required; live board reads are not implemented")?;
    let mut snapshot: Snapshot = serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
    )
    .context("decode snapshot")?;
    snapshot.validate()?;
    policy.normalize(&mut snapshot)?;
    Ok(snapshot)
}

fn execute(cli: Cli, runtime: &Runtime) -> Result<()> {
    ensure!(
        cli.template.is_none(),
        "unsupported_capability: --template is not implemented"
    );
    if let board_forge::forgejo::Board::Forgejo(config) = board(&cli)?
        && let Some(warning) = config.plain_http_warning()
    {
        eprintln!("{warning}");
    }
    match &cli.command {
        Command::HelpMarkdown => {
            use clap::CommandFactory;
            io::stdout()
                .write_all(cli_docs::markdown(Cli::command()).as_bytes())
                .context("write CLI reference")
        }
        Command::Action {
            command: Action::Check(args),
        } => {
            ensure!(
                cli.json.is_none() && cli.snapshot.is_none(),
                "action check does not support --json or --snapshot"
            );
            action::check(
                args,
                cli.repo.as_deref(),
                &cli.hostname,
                board_forge::github::Client::from_env,
                &mut io::stdout().lock(),
            )
        }
        Command::Check {
            command:
                Check::Proposals {
                    identity,
                    proposals,
                },
        } => {
            use std::io::Read;
            ensure!(
                cli.snapshot.is_some(),
                "proposal checks require --snapshot FILE from a trusted reader"
            );
            let policy = agent_policy(&cli)?;
            let mut input = String::new();
            match proposals {
                Some(path) => {
                    input = std::fs::read_to_string(path)
                        .with_context(|| format!("read proposals {}", path.display()))?
                }
                None => {
                    io::stdin()
                        .read_to_string(&mut input)
                        .context("read proposals from stdin")?;
                }
            }
            let outputs: Vec<Value> = input
                .lines()
                .enumerate()
                .map(|(index, line)| {
                    serde_json::from_str(line)
                        .with_context(|| format!("decode proposal line {}", index + 1))
                })
                .collect::<Result<_>>()?;
            board_core::check_proposals(&policy, identity, &load(&cli, runtime)?, &outputs)
        }
        Command::Request {
            command: Request::Dispatch(args),
        } => {
            ensure!(
                cli.json.is_none(),
                "dispatch prints its own JSON command schema"
            );
            dispatch::prepare(
                &load(&cli, runtime)?,
                args,
                cli.repo.as_deref(),
                &cli.hostname,
            )
        }
        Command::Dispatch(args) => {
            ensure!(
                cli.json.is_none(),
                "dispatch prints the request as JSON; --json is unsupported"
            );
            ensure!(
                cli.snapshot.is_none(),
                "plans are read from the issue's comments, not a snapshot"
            );
            dispatch::from_plan(
                args,
                cli.repo.as_deref(),
                &cli.hostname,
                board_forge::github::Client::from_env,
                || board_forge::github::Client::from_var("DISPATCH_TOKEN"),
                &mut io::stdout().lock(),
            )
        }
        Command::Epic {
            command: Epic::Apply(args),
        } => {
            ensure!(
                cli.json.is_none() && cli.snapshot.is_none(),
                "epic apply does not support --json or --snapshot"
            );
            epic::apply(
                args,
                cli.repo.as_deref(),
                &cli.hostname,
                board_forge::github::Client::from_env,
                &mut io::stdout().lock(),
            )
        }
        Command::Review {
            command: Review::Send(args),
        } => {
            ensure!(
                cli.json.is_none() && cli.snapshot.is_none(),
                "review send does not support --json or --snapshot"
            );
            review::send(
                args,
                cli.repo.as_deref(),
                &cli.hostname,
                board_forge::github::Client::from_env,
                &mut io::stdout().lock(),
            )
        }
        Command::Epic {
            command: Epic::Plan(args),
        } => {
            ensure!(
                cli.json.is_none(),
                "epic plan prints mutations as JSON lines; --json is unsupported"
            );
            ensure!(
                cli.snapshot.is_none(),
                "epic plans are read from the epic's comments, not a snapshot"
            );
            epic::plan(
                args,
                cli.repo.as_deref(),
                &cli.hostname,
                board_forge::github::Client::from_env,
                &mut io::stdout().lock(),
            )
        }
        Command::Run {
            command: Run::Record(args),
        } => {
            ensure!(
                cli.json.is_none(),
                "record prints JSONL proposals; --json is unsupported"
            );
            ensure!(
                !args.apply || cli.snapshot.is_none(),
                "--apply writes to the live board; it cannot use --snapshot"
            );
            let snapshot = load(&cli, runtime)?;
            // Lookup is lazy: existing Run and explicit --run retain the offline path.
            let mut args = args.clone();
            if args.needs_lookup(&snapshot, cli.repo.as_deref(), &cli.hostname)? {
                dispatch::discover(
                    &snapshot,
                    &mut args,
                    cli.repo.as_deref(),
                    &cli.hostname,
                    &board_forge::github::Client::from_env()?,
                    &mut io::stdout().lock(),
                )?;
            }
            if args.apply {
                dispatch::apply(
                    &github(&cli)?,
                    &snapshot,
                    &args,
                    cli.repo.as_deref(),
                    &cli.hostname,
                    &mut io::stdout().lock(),
                )
            } else {
                dispatch::record(&snapshot, &args, cli.repo.as_deref(), &cli.hostname)
            }
        }
        Command::Snapshot {
            command: SnapshotCommand::Create,
        } => render(serde_json::to_value(load(&cli, runtime)?)?, &cli),
        Command::Project {
            command: Project::View,
        } => {
            let snapshot = load(&cli, runtime)?;
            if cli.json.is_some() {
                render(serde_json::to_value(snapshot)?, &cli)
            } else {
                project_summary(&snapshot)
            }
        }
        Command::Project {
            command: Project::Init,
        } => {
            ensure!(cli.json.is_none(), "project init prints a field table");
            if cli.snapshot.is_some() {
                dispatch::init_plan(&load(&cli, runtime)?, &mut io::stdout().lock())
            } else {
                dispatch::init(&github(&cli)?, &mut io::stdout().lock())
            }
        }
        Command::Changes {
            since,
            until,
            format,
            public_only,
            max_chars,
        } => {
            ensure!(cli.snapshot.is_none(), "changes requires a live project");
            ensure!(
                cli.json.is_none(),
                "changes takes --format json, not --json"
            );
            ensure!(
                format == "markdown" || max_chars.is_none(),
                "--max-chars trims Markdown, not --format json"
            );
            let changes = github(&cli)?.changes(
                board_forge::changes::parse_time(since)?,
                until
                    .as_deref()
                    .map(board_forge::changes::parse_time)
                    .transpose()?,
                configured_operator(&cli)?.as_deref(),
                *public_only,
            )?;
            let mut out = io::stdout().lock();
            if format == "json" {
                writeln!(out, "{}", serde_json::to_string_pretty(&changes)?)?;
            } else if let Some(limit) = max_chars {
                write!(out, "{}", changes.markdown_within(*limit)?)?;
            } else {
                write!(out, "{}", changes.markdown())?;
            }
            Ok(())
        }
        Command::Project {
            command: Project::StatusUpdate { status, body },
        } => {
            ensure!(
                cli.snapshot.is_none(),
                "status-update requires a live project"
            );
            ensure!(cli.json.is_none(), "status-update does not support --json");
            let body = body
                .as_ref()
                .map(|path| {
                    use std::io::Read;
                    let mut bytes = Vec::new();
                    std::fs::File::open(path)
                        .with_context(|| format!("open {}", path.display()))?
                        .take((board_forge::github::STATUS_BODY_LIMIT * 4 + 1) as u64)
                        .read_to_end(&mut bytes)?;
                    String::from_utf8(bytes).context("status body must be UTF-8")
                })
                .transpose()?;
            let id = github(&cli)?.status_update(
                status,
                body.as_deref(),
                configured_operator(&cli)?.as_deref(),
            )?;
            writeln!(io::stdout().lock(), "{id}")?;
            Ok(())
        }
        Command::Item {
            command: Item::Search(args),
        } => {
            item_read_check(&cli)?;
            item::list_issues(
                &args.read,
                &board_forge::items::Select::Search(&args.text),
                args.limit,
                board_forge::github::Client::from_env,
                &mut io::stdout().lock(),
            )
        }
        Command::Item {
            command: Item::List(args),
        } => {
            if args.off_board {
                item_read_check(&cli)?;
                return item::list_issues(
                    &args.read,
                    &board_forge::items::Select::OffBoard,
                    args.limit,
                    board_forge::github::Client::from_env,
                    &mut io::stdout().lock(),
                );
            }
            ensure!(
                args.read.config.is_none() && args.read.cache.is_none(),
                "--config and --cache are for --off-board"
            );
            ensure!(
                ["open", "closed", "all"].contains(&args.state.as_str()),
                "--state must be open, closed or all"
            );
            let snapshot = load(&cli, runtime)?;
            let items: Vec<_> = snapshot
                .items
                .iter()
                .filter(|item| {
                    (args.state == "all" || item.state == args.state)
                        && matches_filter(&item.status, &args.status)
                        && matches_filter(&item.priority, &args.priority)
                        && matches_filter(&item.turn, &args.turn)
                        && cli
                            .repo
                            .as_ref()
                            .is_none_or(|repo| *repo == item.identity.repository)
                })
                .collect();
            print_items(&cli, items, args.limit)
        }
        Command::Ask {
            command: Ask::List { limit },
        } => {
            let snapshot = load(&cli, runtime)?;
            let items = snapshot
                .items
                .iter()
                .filter(|item| {
                    item.state == "open"
                        && item.labels.iter().any(|l| l == ask::ASK_LABEL)
                        && cli
                            .repo
                            .as_ref()
                            .is_none_or(|repo| *repo == item.identity.repository)
                })
                .collect();
            print_items(&cli, items, *limit)
        }
        Command::Ask {
            command: Ask::Create(args),
        } => {
            ensure!(cli.json.is_none(), "--json is unsupported for ask create");
            let blocked = item::issue_identity(&args.item, cli.repo.as_deref(), &cli.hostname)?;
            // The ask goes where -R says, else beside the item it blocks.
            let repository = cli.repo.as_deref().unwrap_or(&blocked.repository);
            let composed = ask::compose(args, &blocked, repository)?;
            let mut out = io::stdout().lock();
            if cli.snapshot.is_some() {
                ensure!(
                    !args.apply,
                    "--apply writes to the live board; it cannot use --snapshot"
                );
                return ask::create_preview(
                    &load(&cli, runtime)?,
                    &blocked,
                    repository,
                    args,
                    &composed,
                    &mut out,
                );
            }
            ask::create(
                &github(&cli)?,
                &blocked,
                repository,
                args,
                &composed,
                &mut out,
            )
        }
        Command::Ask {
            command: Ask::Add(args),
        } => {
            decisions::cli_check(cli.snapshot.is_some(), &cli.hostname, cli.json.is_some())?;
            decisions::run(&args.settings, decisions::Operation::Add(args), args.apply)
        }
        Command::Ask {
            command: Ask::Read(settings),
        } => {
            decisions::cli_check(cli.snapshot.is_some(), &cli.hostname, cli.json.is_some())?;
            decisions::run(settings, decisions::Operation::Read, false)
        }
        Command::Ask {
            command: Ask::Resolve(args),
        } => {
            if args
                .item
                .strip_prefix('D')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
            {
                decisions::cli_check(cli.snapshot.is_some(), &cli.hostname, cli.json.is_some())?;
                return decisions::run(
                    &args.settings,
                    decisions::Operation::Resolve(&args.item, &args.answer),
                    args.apply,
                );
            }
            ensure!(cli.json.is_none(), "--json is unsupported for ask resolve");
            let identity = item::issue_identity(&args.item, cli.repo.as_deref(), &cli.hostname)?;
            ask::check_answer(&args.answer)?;
            let mut out = io::stdout().lock();
            if cli.snapshot.is_some() {
                ensure!(
                    !args.apply,
                    "--apply writes to the live board; it cannot use --snapshot"
                );
                return ask::resolve_preview(
                    &load(&cli, runtime)?,
                    &identity,
                    &args.answer,
                    &mut out,
                );
            }
            ask::resolve(
                &github(&cli)?,
                &identity,
                &args.answer,
                args.apply,
                &mut out,
            )
        }
        Command::Item {
            command: Item::View { item },
        } => {
            let snapshot = load(&cli, runtime)?;
            let identity = parse_github_item(item, cli.repo.as_deref(), &cli.hostname)?;
            let item = snapshot
                .items
                .iter()
                .find(|item| item.identity == identity)
                .context("item not found in snapshot (not evidence of forge absence)")?;
            if cli.json.is_some() {
                render(item_json(item)?, &cli)
            } else {
                writeln!(
                    io::stdout().lock(),
                    "{}: {}\nStatus: {}\nTurn: {}",
                    item.identity.display(),
                    item.title,
                    label(&item.status),
                    label(&item.turn)
                )?;
                Ok(())
            }
        }
        Command::Item {
            command: Item::Add(add),
        } => item_write(
            &cli,
            runtime,
            &add.item,
            &add.edits(),
            item::Mode::Add,
            add.apply,
        ),
        Command::Item {
            command:
                Item::Edit(item::Edit {
                    item,
                    fields,
                    apply,
                }),
        } => item_write(&cli, runtime, item, fields, item::Mode::Edit, *apply),
        Command::Reconcile {
            command: Reconcile::Plan { emit, config },
        } => {
            let snapshot = load(&cli, runtime)?;
            // Offline scope is only a proposal; independent policy checking is mandatory.
            let policy = Policy {
                project: snapshot.project.clone(),
                repositories: snapshot
                    .items
                    .iter()
                    .map(|item| item.identity.repository.clone())
                    .collect(),
                capabilities: vec![Capability::ProjectFieldEdit, Capability::CloseIssue],
            };
            let plan = if let Some(path) = config {
                let value: Value = serde_json::from_slice(&std::fs::read(path)?)?;
                let settings = board_forge::status::configured(&value)?;
                if settings.enabled {
                    board_forge::status::plan(
                        &board_forge::github::Client::from_env()?,
                        &snapshot,
                        &settings,
                        value["operator"]
                            .as_str()
                            .context("config missing operator")?,
                    )?
                } else {
                    board_core::reconcile(&snapshot, &policy, &snapshot.clock)?
                }
            } else {
                board_core::reconcile(&snapshot, &policy, &snapshot.clock)?
            };
            if *emit {
                ensure!(cli.json.is_none(), "--emit cannot be combined with --json");
                for (target, diagnostic) in &plan.diagnostics {
                    eprintln!("{target}: {diagnostic}");
                }
                let mut out = io::stdout().lock();
                for output in GitHubSafeOutputs.lower(&plan)? {
                    serde_json::to_writer(&mut out, &output)?;
                    writeln!(out)?;
                }
                eprintln!(
                    "Proposed only; not applied. Independent bounds/check/apply support is required."
                );
                Ok(())
            } else {
                eprintln!("Preview only; not applied.");
                render(serde_json::to_value(plan)?, &cli)
            }
        }
        Command::ReleaseTrain(args) => {
            ensure!(
                cli.json.is_none()
                    && cli.repo.is_none()
                    && cli.snapshot.is_none()
                    && cli.owner.is_none()
                    && cli.project.is_none(),
                "release-train prints lines and reads its settings from --config only"
            );
            ensure!(
                cli.hostname == board_forge::github::DEFAULT_HOST,
                "release-train supports only github.com"
            );
            release_train::run(
                args,
                board_forge::github::Client::from_env,
                &mut io::stdout().lock(),
            )
        }
        Command::Broker {
            command: Broker::Probe(args),
        } => {
            ensure!(
                cli.json.is_none()
                    && cli.repo.is_none()
                    && cli.snapshot.is_none()
                    && cli.owner.is_none()
                    && cli.project.is_none(),
                "broker probe prints lines and reads its settings from --config only"
            );
            ensure!(
                cli.hostname == board_forge::github::DEFAULT_HOST,
                "broker probe supports only github.com"
            );
            broker::run(
                args,
                board_forge::github::Client::from_env,
                &mut io::stdout().lock(),
            )
        }
        Command::Poll(args) => {
            ensure!(
                cli.json.is_none(),
                "poll prints lines; --json is unsupported"
            );
            ensure!(
                cli.repo.is_none(),
                "poll takes --follow and --watch, not --repo"
            );
            poll::run(args, || {
                // Every repository on the board, upstream included, is watched.
                if cli.snapshot.is_none() && cli.owner.is_none() && cli.project.is_none() {
                    return Ok(None);
                }
                Ok(Some(load(&cli, runtime)?))
            })
        }
        Command::Pass(args) => {
            ensure!(
                cli.json.is_none()
                    && cli.repo.is_none()
                    && cli.snapshot.is_none()
                    && cli.owner.is_none()
                    && cli.project.is_none()
                    && cli.board_config.is_none(),
                "pass reads its settings from --config only"
            );
            pass::run(args, runtime, &mut io::stdout().lock())
        }
        Command::Budget(args) => {
            ensure!(
                cli.json.is_none()
                    && cli.repo.is_none()
                    && cli.snapshot.is_none()
                    && cli.owner.is_none()
                    && cli.project.is_none()
                    && cli.board_config.is_none(),
                "budget reads its settings from --config only"
            );
            ensure!(
                cli.hostname == board_forge::github::DEFAULT_HOST,
                "unsupported GitHub host"
            );
            budget::run(
                args,
                board_forge::github::Client::from_env,
                &mut io::stdout().lock(),
            )
        }
        Command::Prune(args) => {
            ensure!(
                cli.json.is_none()
                    && cli.repo.is_none()
                    && cli.snapshot.is_none()
                    && cli.owner.is_none()
                    && cli.project.is_none()
                    && cli.board_config.is_none(),
                "prune reads its settings from --config only, and takes --format json, not --json"
            );
            ensure!(
                cli.hostname == board_forge::github::DEFAULT_HOST,
                "unsupported GitHub host"
            );
            prune::run(
                args,
                board_forge::github::Client::from_env,
                &mut io::stdout().lock(),
            )
        }
        Command::Supervise(args) => {
            ensure!(
                cli.json.is_none() && cli.repo.is_none() && cli.snapshot.is_none(),
                "supervise reads its settings from --config and --supervisor-config only"
            );
            supervise::run(args)
        }
        Command::Status(args) => {
            ensure!(
                cli.repo.is_none() && cli.snapshot.is_none(),
                "status reads live: --owner, --config and optionally --project"
            );
            ensure!(
                cli.hostname == board_forge::github::DEFAULT_HOST,
                "unsupported GitHub host"
            );
            let owner = cli.owner.as_deref().context("status requires --owner")?;
            let settings = org_status::settings(args, owner)?;
            for field in cli.json.iter().flat_map(|f| f.split(',')) {
                ensure!(
                    board_forge::org_status::FIELDS.contains(&field),
                    "unknown JSON field: {field}; available fields: {}",
                    board_forge::org_status::FIELDS.join(",")
                );
                ensure!(
                    field != "board" || cli.project.is_some(),
                    "--json board needs --project"
                );
                ensure!(
                    field != "pools" || settings.pace.is_some(),
                    "--json pools needs the config's pace"
                );
            }
            // Refused before any read, as every other section needs the token too.
            let client = board_forge::github::Client::from_env()?;
            let board = cli.project.map(|_| load(&cli, runtime));
            let report = org_status::report(&client, &settings, board);
            let complete = board_forge::org_status::complete(&report);
            if cli.json.is_some() {
                render(report, &cli)?;
            } else {
                io::stdout()
                    .lock()
                    .write_all(board_forge::org_status::text(&report).as_bytes())?;
            }
            // The report is printed whole; a section that could not be read fails the command.
            ensure!(complete, "status: not every section could be read");
            Ok(())
        }
        Command::Apps {
            command: Apps::Create(args),
        } => {
            ensure!(
                cli.json.is_none()
                    && cli.repo.is_none()
                    && cli.snapshot.is_none()
                    && cli.owner.is_none()
                    && cli.project.is_none()
                    && cli.policy.is_none(),
                "apps create takes only its own options"
            );
            ensure!(
                cli.hostname == board_forge::github::DEFAULT_HOST,
                "unsupported_capability: apps create supports only {}",
                board_forge::github::DEFAULT_HOST
            );
            apps::create(args)
        }
        Command::Test {
            command: Test::Project(options),
        } => {
            ensure!(cli.snapshot.is_none(), "live fixture cannot use --snapshot");
            live::run(
                runtime,
                options,
                cli.owner.as_deref(),
                cli.repo.as_deref(),
                cli.project,
            )
        }
        _ => bail!(
            "unsupported_capability: command is not implemented; use gh issue create or gh issue close for manual changes"
        ),
    }
}

fn item_write(
    cli: &Cli,
    runtime: &Runtime,
    input: &str,
    edits: &[String],
    mode: item::Mode,
    apply: bool,
) -> Result<()> {
    ensure!(cli.json.is_none(), "--json is unsupported for item writes");
    let identity = item::issue_identity(input, cli.repo.as_deref(), &cli.hostname)?;
    let mut out = io::stdout().lock();
    if cli.snapshot.is_some() {
        ensure!(
            !apply,
            "--apply writes to the live board; it cannot use --snapshot"
        );
        return item::preview(&load(cli, runtime)?, &identity, edits, mode, &mut out);
    }
    item::write(
        &github(cli)?,
        &identity,
        edits,
        mode,
        apply,
        &mut None,
        &mut out,
    )
}

/// `item search` and `item list --off-board` read the board and repositories
/// --config names, as prune does, and print text only.
fn item_read_check(cli: &Cli) -> Result<()> {
    ensure!(
        cli.json.is_none()
            && cli.repo.is_none()
            && cli.snapshot.is_none()
            && cli.owner.is_none()
            && cli.project.is_none()
            && cli.board_config.is_none(),
        "this item read takes its board and repositories from --config only, and prints text, not --json"
    );
    ensure!(
        cli.hostname == board_forge::github::DEFAULT_HOST,
        "unsupported GitHub host"
    );
    Ok(())
}

fn print_items(cli: &Cli, mut items: Vec<&board_core::Item>, limit: usize) -> Result<()> {
    items.sort_by(|a, b| a.identity.cmp(&b.identity));
    items.truncate(limit);
    let values = items
        .iter()
        .map(|item| item_json(item))
        .collect::<Result<Vec<_>>>()?;
    if cli.json.is_some() {
        return render(Value::Array(values), cli);
    }
    let mut out = io::stdout().lock();
    writeln!(out, "ITEM\tSTATUS\tPRIORITY\tTURN\tTITLE")?;
    for item in items {
        writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}",
            item.identity.display(),
            label(&item.status),
            label(&item.priority),
            label(&item.turn),
            item.title
        )?;
    }
    Ok(())
}

fn project_summary(snapshot: &Snapshot) -> Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "Project: {}", snapshot.project)?;
    writeln!(out, "Items: {}", snapshot.items.len())?;
    writeln!(
        out,
        "Coverage: {} (observed {})",
        if snapshot.coverage.complete {
            "complete"
        } else {
            "incomplete"
        },
        snapshot.coverage.observed_at
    )?;
    let mut counts = std::collections::BTreeMap::new();
    for item in &snapshot.items {
        *counts.entry(label(&item.status)).or_insert(0_usize) += 1;
    }
    writeln!(out, "STATUS\tITEMS")?;
    for (status, count) in counts {
        writeln!(out, "{status}\t{count}")?;
    }
    writeln!(out, "FIELD\tOPTIONS")?;
    for name in ["Status", "Priority", "Turn"] {
        let field = snapshot
            .fields
            .iter()
            .find(|field| field["name"].as_str() == Some(name));
        let description = match field {
            None => "missing (or schema unavailable in snapshot)".to_owned(),
            Some(field) => field["options"]
                .as_array()
                .filter(|options| !options.is_empty())
                .map(|options| {
                    options
                        .iter()
                        .filter_map(|option| option["name"].as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_else(|| "no single-select options".into()),
        };
        writeln!(out, "{name}\t{description}")?;
    }
    writeln!(
        out,
        "Built-in workflows: not observed; check project settings for overlapping automation"
    )?;
    for option in [
        "Triage",
        "Backlog",
        "Todo",
        "In Progress",
        "Draft",
        "In Review",
        "Blocked",
        "Done",
        "Cancelled",
    ] {
        let available = snapshot.has_status_option(option);
        if !available {
            writeln!(out, "Schema drift: Status has no option {option}")?;
        }
    }
    Ok(())
}

fn label<T: serde::Serialize>(value: &Option<T>) -> String {
    value
        .as_ref()
        .and_then(|v| serde_json::to_value(v).ok())
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "-".into())
}

fn matches_filter<T: serde::Serialize>(value: &Option<T>, filter: &Option<String>) -> bool {
    filter.as_ref().is_none_or(|filter| label(value) == *filter)
}

fn item_json(item: &board_core::Item) -> Result<Value> {
    let mut value = serde_json::to_value(item)?;
    value["number"] = item.identity.number.into();
    value["repository"] = item.identity.repository.clone().into();
    let url = board_forge::github_item_url(&item.identity)?;
    value["url"] = if item.content_kind == "pull_request" {
        url.replace("/issues/", "/pull/")
    } else {
        url
    }
    .into();
    Ok(value)
}

fn select(value: &Value, fields: &[&str]) -> Result<Value> {
    if let Some(array) = value.as_array() {
        return array
            .iter()
            .map(|v| select(v, fields))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array);
    }
    let object = value
        .as_object()
        .context("field selection requires an object")?;
    let mut selected = serde_json::Map::new();
    for field in fields {
        selected.insert(
            (*field).into(),
            object
                .get(*field)
                .with_context(|| format!("unknown JSON field: {field}"))?
                .clone(),
        );
    }
    Ok(Value::Object(selected))
}

fn render(mut value: Value, cli: &Cli) -> Result<()> {
    if let Some(fields) = &cli.json {
        let available: &[&str] = if matches!(cli.command, Command::Status(_)) {
            board_forge::org_status::FIELDS
        } else if matches!(cli.command, Command::Item { .. } | Command::Ask { .. }) {
            &[
                "identity",
                "title",
                "state",
                "state_reason",
                "status",
                "priority",
                "turn",
                "fields_complete",
                "evidence",
                "number",
                "repository",
                "url",
                "content_kind",
                "assignment",
                "labels",
                "assignees",
                "timestamps",
                "fields",
            ]
        } else {
            &[
                "schema",
                "project",
                "actions",
                "diagnostics",
                "clock",
                "coverage",
                "items",
                "fields",
            ]
        };
        for field in fields.split(',') {
            ensure!(
                available.contains(&field),
                "unknown JSON field: {field}; available fields: {}",
                available.join(",")
            );
        }
        value = select(&value, &fields.split(',').collect::<Vec<_>>())?;
    }
    let mut out = io::stdout().lock();
    if let Some(expression) = &cli.jq {
        let mut context = jaq_interpret::ParseCtx::new(Vec::new());
        context.insert_natives(jaq_core::core());
        context.insert_defs(jaq_std::std());
        let (parsed, errors) = jaq_parse::parse(expression, jaq_parse::main());
        ensure!(errors.is_empty(), "invalid --jq expression: {errors:?}");
        let filter = context.compile(parsed.context("empty --jq expression")?);
        ensure!(
            context.errs.is_empty(),
            "invalid --jq expression: unknown function or invalid binding"
        );
        let inputs = jaq_interpret::RcIter::new(std::iter::empty());
        for result in filter.run((jaq_interpret::Ctx::new([], &inputs), value.into())) {
            let result = result.map_err(|error| anyhow::anyhow!("jq: {error}"))?;
            let value: Value = result.into();
            if let Some(string) = value.as_str() {
                writeln!(out, "{string}")?;
            } else {
                writeln!(out, "{value}")?;
            }
        }
    } else {
        serde_json::to_writer_pretty(&mut out, &value)?;
        writeln!(out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    /// Every visible command and argument must say what it does in --help;
    /// unimplemented commands stay hidden instead.
    #[test]
    fn help_describes_every_visible_command_and_argument() {
        fn walk(command: &clap::Command, path: &str, missing: &mut Vec<String>) {
            for arg in command.get_arguments() {
                if !arg.is_hide_set() && arg.get_help().is_none() {
                    missing.push(format!("{path} {}", arg.get_id()));
                }
            }
            for sub in command.get_subcommands() {
                if sub.is_hide_set() {
                    continue;
                }
                let path = format!("{path} {}", sub.get_name());
                if sub.get_about().is_none() {
                    missing.push(path.clone());
                }
                walk(sub, &path, missing);
            }
        }
        let mut command = super::Cli::command();
        command.build();
        let mut missing = Vec::new();
        walk(&command, "agent-board", &mut missing);
        assert!(missing.is_empty(), "undocumented in --help: {missing:?}");
    }
}
