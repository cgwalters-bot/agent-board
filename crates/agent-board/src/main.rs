mod action;
mod ask;
mod decisions;
mod dispatch;
mod epic;
mod item;
mod live;
mod poll;
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
    /// Prepare run field updates (proposals only).
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
    /// The controller's supervisor: a read-only health pass, whether its agent runs, and the executor of its dispatch requests.
    Supervise(supervise::Supervise),
    /// Run the credential-bearing staging integration test.
    Test {
        #[command(subcommand)]
        command: Test,
    },
}

#[derive(Subcommand)]
enum Epic {
    /// Print the mutations the newest accepted epic plan on an epic would make (--dry-run only).
    Plan(epic::Plan),
    /// Apply the newest accepted epic plan once (dry run unless --apply).
    Apply(epic::Apply),
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
        /// Overall project health: on-track, at-risk or off-track.
        #[arg(long, value_parser = ["on-track", "at-risk", "off-track"])]
        status: String,
        /// Read the update body from a UTF-8 file; otherwise summarize since the last update.
        #[arg(long)]
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
    /// Filter by priority: P0, P1 or P2.
    #[arg(long)]
    priority: Option<String>,
    /// Filter by whose turn it is (for example Coordinator).
    #[arg(long)]
    turn: Option<String>,
    /// Filter issue state: open, closed or all.
    #[arg(long, default_value = "all")]
    state: String,
    /// Maximum number of items to display.
    #[arg(short = 'L', long, default_value_t = 30)]
    limit: usize,
}

#[derive(Subcommand)]
enum Ask {
    /// Append to the configured decisions document (preview unless --apply).
    Add(decisions::Add),
    /// Report edits since the bot's last write to the decisions document.
    Read(decisions::Settings),
    /// Open a question issue and set it and the blocked item Needs human (writes with --apply).
    Create(ask::Create),
    /// List open board issues labelled question.
    List {
        /// Maximum number of asks to display.
        #[arg(short = 'L', long, default_value_t = 30)]
        limit: usize,
    },
    /// Comment the answer, close the ask and set it Done (writes with --apply).
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
        #[arg(long)]
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
        #[arg(long)]
        proposals: Option<PathBuf>,
    },
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
    if let Err(error) = execute(cli) {
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

fn load(cli: &Cli) -> Result<Snapshot> {
    let policy = agent_policy(cli)?;
    if cli.snapshot.is_none() {
        let snapshot = github(cli)?.snapshot_with_policy(&policy)?;
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

fn execute(cli: Cli) -> Result<()> {
    ensure!(
        cli.template.is_none(),
        "unsupported_capability: --template is not implemented"
    );
    match &cli.command {
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
            board_core::check_proposals(&policy, identity, &load(&cli)?, &outputs)
        }
        Command::Request {
            command: Request::Dispatch(args),
        } => {
            ensure!(
                cli.json.is_none(),
                "dispatch prints its own JSON command schema"
            );
            dispatch::prepare(&load(&cli)?, args, cli.repo.as_deref(), &cli.hostname)
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
            dispatch::record(&load(&cli)?, args, cli.repo.as_deref(), &cli.hostname)
        }
        Command::Snapshot {
            command: SnapshotCommand::Create,
        } => render(serde_json::to_value(load(&cli)?)?, &cli),
        Command::Project {
            command: Project::View,
        } => {
            let snapshot = load(&cli)?;
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
                dispatch::init_plan(&load(&cli)?, &mut io::stdout().lock())
            } else {
                dispatch::init(&github(&cli)?, &mut io::stdout().lock())
            }
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
            let id = github(&cli)?.status_update(status, body.as_deref())?;
            writeln!(io::stdout().lock(), "{id}")?;
            Ok(())
        }
        Command::Item {
            command: Item::List(args),
        } => {
            ensure!(
                ["open", "closed", "all"].contains(&args.state.as_str()),
                "--state must be open, closed or all"
            );
            let snapshot = load(&cli)?;
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
            let snapshot = load(&cli)?;
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
                    &load(&cli)?,
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
                return ask::resolve_preview(&load(&cli)?, &identity, &args.answer, &mut out);
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
            let snapshot = load(&cli)?;
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
        } => item_write(&cli, &add.item, &add.edits(), item::Mode::Add, add.apply),
        Command::Item {
            command:
                Item::Edit(item::Edit {
                    item,
                    fields,
                    apply,
                }),
        } => item_write(&cli, item, fields, item::Mode::Edit, *apply),
        Command::Reconcile {
            command: Reconcile::Plan { emit, config },
        } => {
            let snapshot = load(&cli)?;
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
                Ok(Some(load(&cli)?))
            })
        }
        Command::Supervise(args) => {
            ensure!(
                cli.json.is_none() && cli.repo.is_none() && cli.snapshot.is_none(),
                "supervise reads its settings from --config and --supervisor-config only"
            );
            supervise::run(args)
        }
        Command::Test {
            command: Test::Project(options),
        } => {
            ensure!(cli.snapshot.is_none(), "live fixture cannot use --snapshot");
            live::run(
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
        return item::preview(&load(cli)?, &identity, edits, mode, &mut out);
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
        let available: &[&str] =
            if matches!(cli.command, Command::Item { .. } | Command::Ask { .. }) {
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
