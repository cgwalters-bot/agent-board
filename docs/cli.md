# CLI reference

> This text is LLM generated.

Generated from `agent-board`'s own help by `agent-board help-markdown`; regenerate it with `just cli-docs` rather than editing it. The `gh-agent-board` binary is the same CLI, run as `gh agent-board`.

## agent-board

Typed board snapshots and preview-only proposals

```text
agent-board [OPTIONS] <COMMAND>
```

Commands:

- [`intake`](#agent-board-intake): Report new Forgejo issues and comments; no bodies, routing or forge writes
- [`action`](#agent-board-action): Validate proposed operator mutations; never execute them
- [`triage`](#agent-board-triage): Apply an operator-reviewed triage verdict list (preview unless --apply)
- [`check`](#agent-board-check): Check proposals without applying them
- [`project`](#agent-board-project): Inspect a project's summary and schema
- [`changes`](#agent-board-changes): What changed in the followed and midstream repositories of --config since each one's watermark, read-only: a summary by repository, or --json for the whole diff; --commit moves the watermarks (docs/changes.md). With --since, instead, what landed, closed, opened or changed hold on the live project since a time, each line a link
- [`item`](#agent-board-item): List or inspect board items
- [`ask`](#agent-board-ask): Ask the operator a question as an issue, list or resolve asks
- [`run`](#agent-board-run): Propose run field updates, or write them with --apply
- [`request`](#agent-board-request): Prepare an operator-approved workflow dispatch (no network writes)
- [`dispatch`](#agent-board-dispatch): Start the job an issue's newest accepted plan comment proposes, once (--dry-run prints the request)
- [`pools`](#agent-board-pools): Resolve deployment-owned pool settings (offline; no inference or writes)
- [`overlap`](#agent-board-overlap): The open pull requests of -R, of any author, and the implement runs in flight that share a path with a change, with their age and last activity and which drafts are stale, as the dispatch overlap check reads them (read-only; docs/job-plans.md#the-overlap-command)
- [`epic`](#agent-board-epic): Check a coordinator's plan for an epic's sub-issues
- [`review`](#agent-board-review): Send an operator-approved review draft as the bot
- [`snapshot`](#agent-board-snapshot): Save a normalized board snapshot for offline use
- [`reconcile`](#agent-board-reconcile): Preview deterministic board repairs; never apply them
- [`poll`](#agent-board-poll): One pass of the pull request poll, previewed or applied; says when the next is due
- [`pass`](#agent-board-pass): One scheduled pass: follow up pull requests, apply and plan live epics, then choose and start board work by Priority, WIP and pace; previews unless --apply (docs/pass.md)
- [`admit`](#agent-board-admit): Take a lease on a free slot for a run before any inference, reaping an ended or expired lease to take its slot, or report the one the run holds already; refused, with the reason, when wip, pacing of its pool or the budget leave none (docs/pass.md#leases)
- [`release`](#agent-board-release): Free a run's lease, as its last step
- [`budget`](#agent-board-budget): What each item under the board's open epics has cost, sub-issues included, beside the board's weekly budget and each item's Budget ($) field, or with --split a window's spend split between upstream work and the harness (read-only; docs/pass.md#budget-tree)
- [`cost`](#agent-board-cost): What one UTC day's agent runs spent, by kind, model, repository and item, from each run's agent-run artifact, as agent-board/cost-day/v1 JSON (read-only; docs/cost.md)
- [`ci-health`](#agent-board-ci-health): Rolling 30-day CI observations for explicit repositories/workflows; read-only, opt-in (docs/ci-health.md)
- [`prune`](#agent-board-prune): Open issues of the board's work repositories that are candidates to close (overtaken, duplicate, stale or triaged done), each with its reasons and evidence, unless labelled keep (read-only; docs/prune.md)
- [`promote`](#agent-board-promote): Preview operator /promote command evidence on configured forks, or with promote.onApproval the operator's current-head approval of a draft; standalone, not a pass stage. Policy and human-text provenance must pass. With promote.publish=true, --apply and --max-writes, revalidate all gates and the operator's current-head approval, rebase cleanly and add operator sign-offs where needed, then create an upstream draft or update an existing same-head PR with approved fork title/body, appending a closing reference for the source issue's optional Upstream: #N link. At most one candidate and four writes; one fork status comment and a create-only publication branch, no approved-ref updates, readiness changes or fork closure. Conflicting rebases hold. Bot-fork acceptance still needs recorded staging verification (docs/architecture.md#promote)
- [`supervise`](#agent-board-supervise): The controller's supervisor: a read-only health pass, whether its agent runs, and the executor of its dispatch requests
- [`status`](#agent-board-status): How loaded the --owner organization is: runner jobs, agent runs, pools, merge queues and, with --project, the board (read-only; GH_TOKEN reads)
- [`release-train`](#agent-board-release-train): One pass of the release train: cut a release and move its callers' pins, a step a pass (prints unless --apply)
- [`digest`](#agent-board-digest): Gather weekly public material and file or reuse its issue; preview unless --apply (docs/digest.md)
- [`broker`](#agent-board-broker): Probe the inference broker and keep one outage issue open while it is unreachable
- [`apps`](#agent-board-apps): Create the per-role GitHub Apps (docs/apps.md)
- [`test`](#agent-board-test): Run the credential-bearing staging integration test

Global options:

- `--policy <POLICY>`: Trusted identity policy; defaults to the policy compiled from this repository
- `--snapshot <SNAPSHOT>`: Read an offline board snapshot instead of GitHub
- `--json <FIELDS>`: Print selected comma-separated JSON fields
- `--jq <JQ>`: Filter JSON output with a jq expression
- `-R, --repo <REPO>`: Repository OWNER/REPO for filtering or bare item numbers
- `--hostname <HOSTNAME>`: GitHub hostname (only github.com is supported for live reads) (default: `github.com`)
- `--owner <OWNER>`: Organization or user owning the live project
- `--project <PROJECT>`: Live project number, not its node ID
- `--board-config <FILE>`: Trusted default-branch agent-board.json whose `board` names the forge (GitHub by default)

## agent-board intake

Report new Forgejo issues and comments; no bodies, routing or forge writes

```text
agent-board intake [OPTIONS]
```

Options:

- `--since <SINCE>`: Override stored cursors with this RFC3339 start time
- `--state <FILE>`: JSON map of repository to since cursor; saved after complete reads only

## agent-board action

Validate proposed operator mutations; never execute them

```text
agent-board action [OPTIONS] <COMMAND>
```

Commands:

- [`check`](#agent-board-action-check): Print the newest accepted proposal's typed mutations without writing

## agent-board action check

Print the newest accepted proposal's typed mutations without writing

```text
agent-board action check [OPTIONS] --config <FILE> <ISSUE>
```

Arguments:

- `<ISSUE>`: Issue holding the proposal: URL, OWNER/REPO#N, or N with --repo

Options:

- `--config <FILE>`: Repository-owned configuration from a protected default-branch checkout

## agent-board triage

Apply an operator-reviewed triage verdict list (preview unless --apply)

```text
agent-board triage [OPTIONS] <COMMAND>
```

Commands:

- [`apply`](#agent-board-triage-apply): Comment decisions, remove needs-human/needs-operator and unassign the operator

## agent-board triage apply

Comment decisions, remove needs-human/needs-operator and unassign the operator

```text
agent-board triage apply [OPTIONS] --verdicts <FILE> --operator <LOGIN>
```

Options:

- `--verdicts <FILE>`: Operator-reviewed JSON array of {item, verdict, decision}; not fetched from agent comments
- `--operator <LOGIN>`: Operator login to unassign; other assignees are preserved
- `--dry-run`: Print the planned requests without writing (the default)
- `--apply`: Apply with your GH_TOKEN; this is the operator's acceptance of the list
- `--max-writes <MAX_WRITES>`: Maximum write requests for the whole list, 0..100; refused before any writes if exceeded (default: `20`)

## agent-board check

Check proposals without applying them

```text
agent-board check [OPTIONS] <COMMAND>
```

Commands:

- [`maintain-digest`](#agent-board-check-maintain-digest): Validate a candidate maintain digest offline; no credentials, network or publication
- [`proposals`](#agent-board-check-proposals): Refuse outputs outside an identity's assignment and field policy
- [`session-batch`](#agent-board-check-session-batch): Validate a session run's batch comment offline and print its typed actions; no credentials, network or writes

## agent-board check maintain-digest

Validate a candidate maintain digest offline; no credentials, network or publication

```text
agent-board check maintain-digest [OPTIONS]
```

Options:

- `--body <FILE>`: Candidate Markdown file; reads stdin when omitted

## agent-board check proposals

Refuse outputs outside an identity's assignment and field policy

```text
agent-board check proposals [OPTIONS] --as <IDENTITY>
```

Options:

- `--as <IDENTITY>`: Identity from the policy whose bounds the proposals must stay within
- `--proposals <FILE>`: JSONL proposals file; reads stdin when omitted

## agent-board check session-batch

Validate a session run's batch comment offline and print its typed actions; no credentials, network or writes

```text
agent-board check session-batch [OPTIONS]
```

Options:

- `--body <FILE>`: Candidate comment body holding one agent-board-session block; reads stdin when omitted
- `--allow-repo <OWNER/REPO>`: Repository whose issues a batch may change; repeat for each. From the caller's configuration
- `--priority-option <OPTION>`: An option of the board's Priority field; repeat for each
- `--ask-domain <DOMAIN>`: A domain of the caller's operatorAsk a batch may ask about; repeat for each

## agent-board project

Inspect a project's summary and schema

```text
agent-board project [OPTIONS] <COMMAND>
```

Commands:

- [`view`](#agent-board-project-view): Show item counts and available fields (use --json for snapshot data)
- [`init`](#agent-board-project-init): Create the Turn, Run and Result fields dispatch needs, if missing (writes)
- [`status-update`](#agent-board-project-status-update): Publish a project status update (writes; requires project write permission)

## agent-board project view

Show item counts and available fields (use --json for snapshot data)

```text
agent-board project view [OPTIONS]
```

## agent-board project init

Create the Turn, Run and Result fields dispatch needs, if missing (writes)

```text
agent-board project init [OPTIONS]
```

## agent-board project status-update

Publish a project status update (writes; requires project write permission)

```text
agent-board project status-update [OPTIONS] --status <STATUS>
```

Options:

- `--status <STATUS>`: Overall project health (possible values: `on-track`, `at-risk`, `off-track`)
- `--body <FILE>`: Read the update body from a UTF-8 file; otherwise post the changes since the last update

## agent-board changes

What changed in the followed and midstream repositories of --config since each one's watermark, read-only: a summary by repository, or --json for the whole diff; --commit moves the watermarks (docs/changes.md). With --since, instead, what landed, closed, opened or changed hold on the live project since a time, each line a link

```text
agent-board changes [OPTIONS]
```

Options:

- `--config <FILE>`: The pass configuration (docs/pass.md) whose `follow` list and `midstream` owner name the repositories, and whose `operator` has comments flagged; from a trusted checkout, with credentials from GH_TOKEN, GITHUB_TOKEN or gh auth token [default: .github/pass.json]
- `--state <DIR>`: Where the watermarks and ETags are kept, in changes.json [default: $XDG_CACHE_HOME/agent-board or ~/.cache/agent-board]
- `--commit`: Move every read repository's watermark to this read's time; without it the next run lists the same changes again
- `--replay <FILE>`: Read a recorded diff (a replay fixture) at its clock instead of the forge; needs no token, and --state, so that a fixture's watermarks never land in the cache
- `--record <FILE>`: Read live, and save every read and its answer to FILE as a replay fixture
- `--since <SINCE>`: Instead, what landed, closed, opened or changed hold on the live project (--owner, --project) since this time, RFC 3339 (such as 2026-10-23T00:00:00Z), each line a link; --board-config's operator counts as an author whose new pull requests are listed
- `--until <UNTIL>`: End of --since's window, RFC 3339; without it, the window runs to now
- `--format <FORMAT>`: --since's output format (possible values: `markdown`, `json`) (default: `markdown`)
- `--public-only`: Leave out of --since's list the issues and pull requests of private repositories, for a list that will be published
- `--max-chars <N>`: Trim --since's Markdown to at most N characters as a generated status update is trimmed: the longest sections lose their last lines first, each ending with `+K more`

## agent-board item

List or inspect board items

```text
agent-board item [OPTIONS] <COMMAND>
```

Commands:

- [`list`](#agent-board-item-list): List board items, including closed issues by default
- [`view`](#agent-board-item-view): Inspect an item by URL, OWNER/REPO#N, or N with --repo
- [`search`](#agent-board-item-search): Find open board issues of --config's workRepositories by title or body text (read-only)
- [`add`](#agent-board-item-add): Add an issue to the board, optionally setting Priority (writes with --apply)
- [`edit`](#agent-board-item-edit): Set single-select or text fields on a board issue (writes with --apply)

## agent-board item list

List board items, including closed issues by default

```text
agent-board item list [OPTIONS]
```

Options:

- `--status <STATUS>`: Filter by exact board status (for example "In Progress")
- `--priority <PRIORITY>`: Filter by priority: P0 to P3
- `--turn <TURN>`: Filter by whose turn it is (for example Coordinator)
- `--state <STATE>`: Filter by issue state (possible values: `open`, `closed`, `all`) (default: `all`)
- `-L, --limit <LIMIT>`: Maximum number of items to display (default: `30`)
- `--off-board`: List the open issues of --config's workRepositories that are not on its board instead, without filters
- `--config <FILE>`: The pass configuration (docs/pass.md), from a trusted default-branch checkout: the open issues of its `workRepositories` are read beside its `board`'s. GH_TOKEN reads; nothing is written
- `--cache <FILE>`: The response cache: read if it exists, so unchanged pages are revalidated and cost no rate budget, then replaced by the responses this read used

## agent-board item view

Inspect an item by URL, OWNER/REPO#N, or N with --repo

```text
agent-board item view [OPTIONS] <ITEM>
```

Arguments:

- `<ITEM>`: Item URL, OWNER/REPO#N, or N with --repo

## agent-board item search

Find open board issues of --config's workRepositories by title or body text (read-only)

```text
agent-board item search [OPTIONS] <TEXT>
```

Arguments:

- `<TEXT>`: Text to find in a title or body, ignoring case

Options:

- `--config <FILE>`: The pass configuration (docs/pass.md), from a trusted default-branch checkout: the open issues of its `workRepositories` are read beside its `board`'s. GH_TOKEN reads; nothing is written
- `--cache <FILE>`: The response cache: read if it exists, so unchanged pages are revalidated and cost no rate budget, then replaced by the responses this read used
- `-L, --limit <LIMIT>`: Maximum number of items to display (default: `30`)

## agent-board item add

Add an issue to the board, optionally setting Priority (writes with --apply)

```text
agent-board item add [OPTIONS] <ITEM>
```

Arguments:

- `<ITEM>`: Issue URL, OWNER/REPO#N, or N with --repo

Options:

- `--priority <PRIORITY>`: Priority option to set, for example P1
- `--turn <TURN>`: Turn option; defaults to Coordinator when the board offers it
- `--apply`: Write to the live board with your token; without it, only print the proposal

## agent-board item edit

Set single-select or text fields on a board issue (writes with --apply)

```text
agent-board item edit [OPTIONS] --field <NAME=VALUE> <ITEM>
```

Arguments:

- `<ITEM>`: Issue URL, OWNER/REPO#N, or N with --repo

Options:

- `--field <NAME=VALUE>`: Set a single-select or text field, for example Status=Todo (repeatable)
- `--apply`: Write to the live board with your token; without it, only print the proposal

## agent-board ask

Ask the operator a question as an issue, list or resolve asks

```text
agent-board ask [OPTIONS] <COMMAND>
```

Commands:

- [`add`](#agent-board-ask-add): Append to the configured decisions document (preview unless --apply)
- [`read`](#agent-board-ask-read): Report edits since the bot's last write to the decisions document
- [`create`](#agent-board-ask-create): Open a question issue and label it and the blocked item needs-human (writes with --apply)
- [`list`](#agent-board-ask-list): List open board issues labelled question
- [`resolve`](#agent-board-ask-resolve): Comment the answer, close the ask and unlabel the item it blocked (writes with --apply)

## agent-board ask add

Append to the configured decisions document (preview unless --apply)

```text
agent-board ask add [OPTIONS] --context <CONTEXT> --option <OPTIONS> --recommend <RECOMMEND> --item <ITEM> --priority <PRIORITY> <QUESTION>
```

Arguments:

- `<QUESTION>`: Question shown in the decision heading

Options:

- `--config <FILE>`: Caller-owned agent-board.json from a trusted default-branch checkout (default: `agent-board.json`)
- `--context <CONTEXT>`: Background paragraph, without headings or fences
- `--option <OPTIONS>`: Alternative answer (repeat 2 to 10 times)
- `--recommend <RECOMMEND>`: Reason to recommend the first option (A)
- `--item <ITEM>`: Blocked issue, OWNER/REPO#N
- `--priority <PRIORITY>`: Priority of the blocked decision (possible values: `P0`, `P1`, `P2`)
- `--apply`: Update the issue with your token; otherwise only print the merged body

## agent-board ask read

Report edits since the bot's last write to the decisions document

```text
agent-board ask read [OPTIONS]
```

Options:

- `--config <FILE>`: Caller-owned agent-board.json from a trusted default-branch checkout (default: `agent-board.json`)

## agent-board ask create

Open a question issue and label it and the blocked item needs-human (writes with --apply)

```text
agent-board ask create [OPTIONS] --item <ITEM> --assignee <ASSIGNEE> <QUESTION>
```

Arguments:

- `<QUESTION>`: The question to ask

Options:

- `--item <ITEM>`: The board issue the question blocks: URL, OWNER/REPO#N, or N with --repo
- `--assignee <ASSIGNEE>`: The operator's GitHub login; the ask is assigned to them
- `--option <TEXT>`: An alternative to choose from, lettered A, B, ... in order (repeat 2 to 10 times)
- `--recommend <WHY>`: Recommend the first option (A), for this reason
- `--context <CONTEXT>`: Background printed before the question
- `--title <TITLE>`: Issue title; defaults to the question on one line, cut to 100 characters
- `--apply`: Open the issue, add it to the board and label the item with your token; without it, only print proposals

## agent-board ask list

List open board issues labelled question

```text
agent-board ask list [OPTIONS]
```

Options:

- `-L, --limit <LIMIT>`: Maximum number of asks to display (default: `30`)

## agent-board ask resolve

Comment the answer, close the ask and unlabel the item it blocked (writes with --apply)

```text
agent-board ask resolve [OPTIONS] <ITEM> <ANSWER>
```

Arguments:

- `<ITEM>`: The ask: URL, OWNER/REPO#N, or N with --repo
- `<ANSWER>`: The answer, posted as a comment before the ask is closed

Options:

- `--config <FILE>`: Caller-owned agent-board.json from a trusted default-branch checkout (default: `agent-board.json`)
- `--apply`: Comment, close and unlabel with your token; without it, only print proposals

## agent-board run

Propose run field updates, or write them with --apply

```text
agent-board run [OPTIONS] <COMMAND>
```

Commands:

- [`record`](#agent-board-run-record): Propose recording an operator-verified run and optional applied result

## agent-board run record

Propose recording an operator-verified run and optional applied result

```text
agent-board run record [OPTIONS] --item <ITEM>
```

Options:

- `--item <ITEM>`: Dispatched issue: URL, OWNER/REPO#N, or N with --repo
- `--run <RUN>`: Actions URL selected and verified by the operator, not agent prose
- `--result <RESULT>`: Applied pull request or issue comment URL. Omit for admission only
- `--apply`: Write the validated proposal to the live board with your GH_TOKEN
- `--caller <CALLER>`: Dispatch caller repository to search when no Run is recorded
- `--kind <KIND>`: Dispatch kind to match when looking up a run (possible values: `implement`, `triage`, `research`, `session`) (default: `implement`)
- `--yes`: Accept the unique run printed by caller lookup

## agent-board request

Prepare an operator-approved workflow dispatch (no network writes)

```text
agent-board request [OPTIONS] <COMMAND>
```

Commands:

- [`dispatch`](#agent-board-request-dispatch): Print gh argv for one eligible issue; does not execute it

## agent-board request dispatch

Print gh argv for one eligible issue; does not execute it

```text
agent-board request dispatch [OPTIONS] --item <ITEM> --caller <CALLER>
```

Options:

- `--item <ITEM>`: Issue to dispatch: URL, OWNER/REPO#N, or N with --repo
- `--caller <CALLER>`: Repository containing the reviewed dispatch.yml caller, not the target code
- `--branch <BRANCH>`: Caller default branch; the caller independently checks this (default: `main`)
- `--kind <KIND>`: Profile the caller workflow runs with (possible values: `implement`, `triage`, `research`, `session`) (default: `implement`)

## agent-board dispatch

Start the job an issue's newest accepted plan comment proposes, once (--dry-run prints the request)

```text
agent-board dispatch [OPTIONS] --caller <OWNER/REPO> --workflow <FILE> --operator <LOGIN> <ISSUE>
```

Arguments:

- `<ISSUE>`: Issue whose newest accepted plan comment to dispatch: URL, OWNER/REPO#N, or N with --repo. A runner profile name is forwarded unchanged; only the caller resolves it. Hosted keeps the existing default request

Options:

- `--dry-run`: Print the workflow_dispatch request instead of sending it
- `--apply`: Send it, unless this plan has been dispatched already, and record the run in one comment on the issue, with GH_TOKEN; the run is started with DISPATCH_TOKEN if set
- `--caller <OWNER/REPO>`: Repository whose --workflow the request starts; from the caller's configuration
- `--workflow <FILE>`: Workflow file in --caller that takes a plan's inputs, such as dispatch.yml
- `--ref <GIT_REF>`: Ref of the caller to run --workflow from (default: `main`)
- `--dispatch-routing <FILE>`: Trusted namespace routing JSON. Off without it; never read from a job plan
- `--operator <LOGIN>`: Login whose plans count: the operator
- `--planner <LOGIN>`: Login the planning run comments as, whose plans also count
- `--batch <N>`: Add up to N small, ready sub-issues of the issue's epic, each with its own implement plan, to one run; 0 runs the issue alone. From the caller's configuration (default: `0`)
- `--bot <LOGIN>`: Login that opens the implement runs' pull requests: before dispatching, compare the issue with its open pull requests and the runs in flight on the plan's repository, and wait for or run beside them. Off without it; from the caller's configuration
- `--collaborators-only`: Refuse the plan unless each issue the run reads is locked and was written only by accounts with write access to its repository. Off without it; from the caller's configuration
- `--status-app-id <ID>`: With --collaborators-only, also admit issue comments that the GitHub App with this id posted as its bot: the status app (docs/apps.md). From the caller's configuration

## agent-board pools

Resolve deployment-owned pool settings (offline; no inference or writes)

```text
agent-board pools [OPTIONS] <COMMAND>
```

Commands:

- [`resolve`](#agent-board-pools-resolve): Print agent=, model=, budget= and skilled= lines for $GITHUB_OUTPUT

## agent-board pools resolve

Print agent=, model=, budget= and skilled= lines for $GITHUB_OUTPUT.

Append stdout to $GITHUB_OUTPUT. Errors print no outputs. Nothing calls this automatically: use only the caller's trusted pools file, not a target checkout. Every pool's model and positive-integer budget string is validated on load. Heavy changes only the model; no pool and empty effort leave agent, model and budget empty for the wrapper's fallbacks.

```text
agent-board pools resolve [OPTIONS] --pools <FILE> --kind <KIND>
```

Options:

- `--pools <FILE>`: Caller-owned pools.json from a trusted checkout
- `--pool <POOL>`: Explicit pool; empty uses the caller's fallbacks
- `--effort <EFFORT>`: Empty or heavy; heavy requires the selected pool's valid heavy.model
- `--kind <KIND>`: Run kind; review rejects heavy effort
- `--fallback-agent <FALLBACK_AGENT>`: Wrapper's fallback agent, also used to identify an inherited heavy pool (default: `opencode`)
- `--fallback-model <FALLBACK_MODEL>`: Wrapper's fallback model; heavy inheritance needs exactly one matching pool

## agent-board overlap

The open pull requests of -R, of any author, and the implement runs in flight that share a path with a change, with their age and last activity and which drafts are stale, as the dispatch overlap check reads them (read-only; docs/job-plans.md#the-overlap-command)

```text
agent-board overlap [OPTIONS] --caller <OWNER/REPO> --workflow <FILE> <--paths <PATH>...|--branch <BRANCH>|--pr <N>>
```

Options:

- `--paths <PATH>`: Paths the change touches, as files or directories
- `--branch <BRANCH>`: Branch of -R whose changes against its default branch are the change
- `--pr <N>`: Pull request of -R whose files (at most 100) are the change; it is not listed itself
- `--caller <OWNER/REPO>`: Repository whose --workflow runs are the dispatch runs; from the caller's configuration
- `--workflow <FILE>`: Workflow file in --caller that starts the runs, such as dispatch.yml
- `--stale-draft-hours <HOURS>`: Hours a draft's head must be proved unchanged for it to be flagged stale, as the pass's `overlap.staleDraftHours` (default: `4`)

## agent-board epic

Check a coordinator's plan for an epic's sub-issues

```text
agent-board epic [OPTIONS] <COMMAND>
```

Commands:

- [`plan`](#agent-board-epic-plan): Print the mutations the newest accepted epic plan on an epic would make (--dry-run only)
- [`apply`](#agent-board-epic-apply): Apply the newest accepted epic plan once (dry run is the default; --apply writes)

## agent-board epic plan

Print the mutations the newest accepted epic plan on an epic would make (--dry-run only)

```text
agent-board epic plan [OPTIONS] --dry-run --operator <LOGIN> <ISSUE>
```

Arguments:

- `<ISSUE>`: Epic whose newest accepted epic plan to check: URL, OWNER/REPO#N, or N with --repo

Options:

- `--dry-run`: Print the mutations instead of making them (required; use epic apply --apply to make them)
- `--operator <LOGIN>`: Login whose plans count: the operator
- `--planner <LOGIN>`: Login the planning run comments as, whose plans also count
- `--allow-repo <OWNER/REPO>`: Repository whose issues a plan may create or change; repeat for each. From the caller's configuration
- `--priority-option <OPTION>`: An option of the board's Priority field; repeat for each

## agent-board epic apply

Apply the newest accepted epic plan once (dry run is the default; --apply writes)

```text
agent-board epic apply [OPTIONS] --config <FILE> <ISSUE>
```

Arguments:

- `<ISSUE>`: Epic URL, OWNER/REPO#N, or N with --repo

Options:

- `--config <FILE>`: Trusted configuration file from a protected default-branch checkout
- `--apply`: Write with the configured bot's token
- `--expect-request <ID>`: Refuse unless the newest trusted plan has this request identifier
- `--expect-comment <ID>`: Refuse unless the newest trusted plan is this comment
- `--expect-updated-at <RFC3339>`: Refuse unless the plan's last edit (or creation) is this RFC 3339 instant

## agent-board review

Send an operator-approved review draft as the bot

```text
agent-board review [OPTIONS] <COMMAND>
```

Commands:

- [`send`](#agent-board-review-send): Post the newest review draft on an issue once the operator approves it (dry run unless --apply)

## agent-board review send

Post the newest review draft on an issue once the operator approves it (dry run unless --apply)

```text
agent-board review send [OPTIONS] --config <FILE> <ISSUE>
```

Arguments:

- `<ISSUE>`: Draft issue URL, OWNER/REPO#N, or N with --repo

Options:

- `--config <FILE>`: Trusted agent-board.json from a protected default-branch checkout: operator, bot and review
- `--dry-run`: Print the requests without writing (the default)
- `--apply`: Send with the configured bot's token
- `--approved-by-edit <EVENT>`: The operator's approval is their ticking the body's checkbox, not a /send comment: the `issues: edited` event payload of that edit, such as $GITHUB_EVENT_PATH, which binds the send to the body it left

## agent-board snapshot

Save a normalized board snapshot for offline use

```text
agent-board snapshot [OPTIONS] <COMMAND>
```

Commands:

- [`create`](#agent-board-snapshot-create): Write the complete normalized snapshot as JSON to stdout

## agent-board snapshot create

Write the complete normalized snapshot as JSON to stdout

```text
agent-board snapshot create [OPTIONS]
```

## agent-board reconcile

Preview deterministic board repairs; never apply them

```text
agent-board reconcile [OPTIONS] <COMMAND>
```

Commands:

- [`plan`](#agent-board-reconcile-plan): Propose repairs from a live board or offline snapshot

## agent-board reconcile plan

Propose repairs from a live board or offline snapshot

```text
agent-board reconcile plan [OPTIONS]
```

Options:

- `--emit`: Write safe-output proposals as JSONL, not authorized mutations
- `--config <FILE>`: Trusted default-branch agent-board.json; statusReconcile is off by default

## agent-board poll

One pass of the pull request poll, previewed or applied; says when the next is due

```text
agent-board poll [OPTIONS]
```

Options:

- `--config <FILE>`: Repository-owned agent-board.json from a trusted default-branch checkout, never a dispatched pull request or workflow input. File settings win
- `--pools <FILE>`: Repository-owned pools file (each pool's `tier`), from the same trusted checkout, needed only where fixPools or kindPools names a tier such as tier-1
- `--fix-bounds <FILE>`: The caller's agentic-job bounds file for its `fix` kind (such as .github/agentic-job/fix.toml), from the same trusted checkout, never a dispatched head or workflow input: a pull request whose repository is outside its `repos` (unless push_to_pull_request_branch's `own_author` is the pull request's author), or whose head branch is outside its push_to_pull_request_branch `branches`, gets no fix, rebase, continuation or answer round and is left to the operator, nor does a fix round on the operator's comments start there. Repeatable: a pull request is inside where any one file admits both its repository and its head branch. The policy step stays the boundary
- `--dry-run`: Print the step a pass would take for each pull request and take none
- `--apply`: Take the steps: start fix and continuation runs, merge an approved green head, and label needs-human, with GH_TOKEN (the bot's); fix runs are started with DISPATCH_TOKEN if set. Only in --follow repositories, and on the operator's comments where the config's operatorComments names
- `--state <FILE>`: Poll state file (cached responses, the events pages last read, the idle interval); created if missing. Keep it where only the poll writes it: its bodies are trusted
- `--follow <OWNER/REPO>`: Repository whose open pull requests by --bot get a step; its activity is read too. Repeatable
- `--watch <OWNER/REPO>`: Repository whose activity is read, besides those on the board given by --owner/--project or --snapshot. Repeatable
- `--operator <LOGIN>`: Login whose activity is direction and whose review of a head counts
- `--bot <LOGIN>`: Login whose pull requests are followed and whose activity is state
- `--dispatch-repo <OWNER/REPO>`: Repository whose dispatch runs the fix markers name
- `--dispatch-workflow <FILE>`: Workflow file in --dispatch-repo that --apply starts fix runs with
- `--review-workflow <FILE>`: Workflow file of the review job in each --follow repository. Its reviews of a head count as the verdict only with this, and only from a run of it on pull_request_target for that head of a --bot dispatch pull request from its fork into a public repository; without it only --operator's count
- `--fix-pool <POOL>`: The workflow's `pool` input for each fix round in turn. Repeatable; none leaves its default

## agent-board pass

One scheduled pass: follow up pull requests, apply and plan live epics, then choose and start board work by Priority, WIP and pace; previews unless --apply (docs/pass.md)

```text
agent-board pass [OPTIONS]
       agent-board pass <COMMAND>
```

Commands:

- [`next`](#agent-board-pass-next): After an applying pass in GitHub Actions (GITHUB_REPOSITORY, GITHUB_RUN_ID): wait until --wait-minutes after this run started, wake optional stale auxiliary workflows, then dispatch --workflow unless a newer applying run of it on --branch is still to finish; GH_TOKEN needs actions: write (docs/pass.md#the-next-pass)

Options:

- `--config <FILE>`: The pass configuration (docs/pass.md), from a trusted default-branch checkout [default: .github/pass.json in the current directory]. Credentials come from GH_TOKEN, GITHUB_TOKEN or gh auth token (in that order), never the file; DISPATCH_TOKEN overrides the token for starts
- `--fix-bounds <FILE>`: The caller's agentic-job bounds file for its `fix` kind (such as .github/agentic-job/fix.toml), from the same trusted checkout, never a dispatched head or workflow input: a pull request whose repository is outside its `repos` (unless push_to_pull_request_branch's `own_author` is the pull request's author), or whose head branch is outside its push_to_pull_request_branch `branches`, gets no fix, rebase or continuation round and is left to the operator. Repeatable: a pull request is inside where any one file admits both its repository and its head branch. The policy step stays the boundary
- `--apply`: Take the steps: follow up pull requests, publish the usage snapshot, apply epic plans, start the chosen runs, write board Status and, with holds, publish the hold snapshot as GH_TOKEN's bot, starting runs with DISPATCH_TOKEN if set. Without it the pass previews and writes nothing. In a GitHub Actions run (GITHUB_REPOSITORY, GITHUB_RUN_ID), work starts only while no other run of its workflow is in progress. Outside one, it needs both dispatch and write caps (config or flags), and is not serialized against the scheduled workflow
- `--max-dispatch <N>`: Start at most N agent runs in this pass: pull request fix, rebase and continuation rounds, an epic's planning run, and scheduled starts. The rest are held, each line saying so. Overrides config maxDispatch
- `--max-writes <N>`: Take at most N other write steps in this pass: forge writes such as comments, labels, merges, the usage and hold snapshots, epic plans and Status (a pull request's comment, label, retitle, retarget, enqueue or merge step, the usage and hold comments' edits, an epic plan's receipt and each of its actions that writes, and each item's Status change). A step is taken whole or held whole, and an epic plan's steps together. A pass that made progress publishes its hold snapshot one past a full cap above 0, so its progress is kept. Overrides config maxWrites
- `--max-status-changes <N>`: Refuse the board Status stage whole when it would change more than N items' Status [default: config maxStatusChanges, otherwise 50]: the first pass over a board that drifted is looked at first. Pass a higher N once to let it through
- `--replay <FILE>`: Preview a recorded pass (a replay fixture) at its clock instead of reading the forge; needs no token. A Forgejo fixture replays only on the instance it was recorded on. On GitHub, with --record, it seeds the recording instead: its answers stand in for live reads, such as a usage snapshot the token cannot read or the broker's /usage
- `--record <FILE>`: Preview live, and save every read and its answer to FILE as a replay fixture
- `--check-config`: Only check --config and --fix-bounds as a pass would, and exit. Explicit repository lists need no token or forge reads; organization entries need GH_TOKEN to resolve their bounded listing before policy validation. For a caller's pull request CI, so a configuration the pass refuses fails review instead of every pass after merge
- `--verbose`: Show repository resolution details as well as counts

## agent-board pass next

After an applying pass in GitHub Actions (GITHUB_REPOSITORY, GITHUB_RUN_ID): wait until --wait-minutes after this run started, wake optional stale auxiliary workflows, then dispatch --workflow unless a newer applying run of it on --branch is still to finish; GH_TOKEN needs actions: write (docs/pass.md#the-next-pass)

```text
agent-board pass next [OPTIONS] --workflow <FILE> --branch <BRANCH> --apply-title <TITLE> --wait-minutes <MINUTES>
```

Options:

- `--workflow <FILE>`: The pass workflow to dispatch, such as pass.yml; its runs are the ones checked for a newer applying run
- `--branch <BRANCH>`: The default branch: the ref dispatched, and the branch whose runs are read
- `--input <NAME=VALUE>`: An input of the pass dispatch, such as apply=true. Repeatable
- `--apply-title <TITLE>`: The run title of a dispatched pass that applies; a scheduled run always counts as applying
- `--wait-minutes <MINUTES>`: The pass interval: start the next pass this many minutes after this run started, never waiting longer
- `--supervisor-workflow <FILE>`: A supervisor workflow to start, before the pass, when its newest run on --branch, ignoring issue_comment runs, is at least --supervisor-stale-minutes old or is not among its newest 100. Its failures only warn
- `--supervisor-stale-minutes <MINUTES>`: How old the supervisor's newest run may be before it is started
- `--supervisor-input <NAME=VALUE>`: An input of the supervisor dispatch, such as session=false. Repeatable
- `--train-workflow <FILE>`: A release-train workflow to start before the pass when its newest run on --branch is at least --train-stale-minutes old or is not among its newest 100. All run events count. Its failures only warn. Off unless supplied
- `--train-stale-minutes <MINUTES>`: How old the train's newest run may be before it is started; independent of the supervisor
- `--train-input <NAME=VALUE>`: An input of the train dispatch. Repeatable
- `--dry-run`: Print what would wait and start; wait for and start nothing

## agent-board admit

Take a lease on a free slot for a run before any inference, reaping an ended or expired lease to take its slot, or report the one the run holds already; refused, with the reason, when wip, pacing of its pool or the budget leave none (docs/pass.md#leases)

```text
agent-board admit [OPTIONS] --config <FILE> --pool <POOL> --item <ITEM> --run <ID>
```

Options:

- `--config <FILE>`: The pass configuration (docs/pass.md), from a trusted default-branch checkout: its dispatchRepo holds the leases, and its wip, pools, usage and budget size each pool
- `--pool <POOL>`: The pool the run uses, one of the configuration's pools
- `--item <ITEM>`: The item the run works on: URL or OWNER/REPO#N
- `--run <ID>`: The run taking the lease, in dispatchRepo (GITHUB_RUN_ID); a lease is reaped once it ends
- `--override`: Take a slot past wip and pacing; only for GH_TOKEN of the configuration's operator, and said on stdout and in the lease

## agent-board release

Free a run's lease, as its last step

```text
agent-board release [OPTIONS] --config <FILE> --pool <POOL> --run <ID>
```

Options:

- `--config <FILE>`: The pass configuration, as for admit
- `--pool <POOL>`: The pool the run took its lease of
- `--run <ID>`: The run whose lease to free

## agent-board budget

What each item under the board's open epics has cost, sub-issues included, beside the board's weekly budget and each item's Budget ($) field, or with --split a window's spend split between upstream work and the harness (read-only; docs/pass.md#budget-tree)

```text
agent-board budget [OPTIONS] --config <FILE>
```

Options:

- `--config <FILE>`: The pass configuration (docs/pass.md), from a trusted default-branch checkout: its `epics.repository`, `board`, `bot`, `budget.weeklyUsd` and, for --split, `budget.harnessRepositories`. GH_TOKEN reads
- `--split`: Instead of the tree, print the share of the window's public spend on upstream work and on the harness (`budget.harnessRepositories`), in percentages only
- `--four-weeks`: Report the last four complete UTC Monday-to-Monday weeks, read-only. No publishing or admission changes
- `--since <TIME>`: Start of the --split window, RFC 3339 (such as 2026-10-19T00:00:00Z)
- `--until <TIME>`: End of the --split window, or reference clock for --four-weeks, RFC 3339; defaults to now

## agent-board cost

Report one UTC day's agent spend by kind, model, repository and item. Use this for daily accounting, including reviews that are not board items; use budget for the board's epic roll-up instead.

Reads completed GitHub Actions runs created on --date and their agent-run summaries in the pass configuration's workRepositories, follow and dispatchRepo. Supply --config from a trusted default-branch checkout and GH_TOKEN with read access to those repositories' Actions runs and artifacts. The date must be YYYY-MM-DD and not after today (UTC).

Prints agent-board/cost-day/v1 JSON to stdout by default; --human prints a short table. Missing model prices produce null costs, not zero. Unread artifacts are reported under unread; failed requests, exhausted read bounds or cache errors fail the command rather than silently omit runs. Reads are bounded to 1,000 requests and ten minutes.

Never writes to the forge or publishes a report. With --cache, writes summaries locally in an existing directory as RUN_ID.json; cached summaries are not downloaded again. Without --cache, no summaries are saved. The pass's optional cost publication stage is separate; see docs/cost.md for accounting, schema and publication contracts.

```text
agent-board cost [OPTIONS] --config <FILE> --date <DATE>
```

Options:

- `--config <FILE>`: The pass configuration (docs/pass.md), from a trusted default-branch checkout: its `workRepositories`, `follow` and `dispatchRepo`, whose runs count, and its `prices`. GH_TOKEN reads their Actions runs and artifacts
- `--date <DATE>`: The UTC day, YYYY-MM-DD, whose completed runs count; not after today
- `--human`: Print a short table instead of the agent-board/cost-day/v1 JSON
- `--cache <DIR>`: Existing directory that keeps summaries as RUN_ID.json, so a cached run is never downloaded again; omitted by default

```text
Examples:
  Print the JSON report for a completed UTC day:
    agent-board cost --config agent-board.json --date 2026-10-28

  Print a human summary of the same day:
    agent-board cost --config agent-board.json --date 2026-10-28 --human

  Reuse summaries between reads (create the cache directory first):
    agent-board cost --config agent-board.json --date 2026-10-28 --cache cost-cache

  All examples read GitHub and require GH_TOKEN; none publishes a report.
```

## agent-board ci-health

Report rolling 30-day CI observations for explicit GitHub repositories and workflow files. Use this for historical first-attempt rates and recurring failure evidence, not supervise health's one-day signature check or latest-main verdict.

Supply --config from a trusted local checkout and GH_TOKEN or GITHUB_TOKEN with read access to the targets' Actions runs, jobs and logs. There is no organization discovery or implicit board scope. The exclusive --until date must be YYYY-MM-DD and no later than today (UTC); it defaults to today's UTC midnight.

Prints agent-board/ci-health/v1 JSON to stdout by default; --format human prints a concise summary. Exit zero means a report was produced, not that CI is healthy or coverage complete. Check complete at both report and target levels and display warnings before using rates. Invalid configuration or authentication setup fails the command. Consumers must use JSON, not scrape the human format.

Reads are bounded to 500 API operations, a 120-second collection deadline, ten 100-item pages per listing, 1 MiB per log and 32 MiB aggregate decoded bytes. In-flight requests can overrun the deadline. Busy windows and unavailable logs can leave coverage incomplete.

Nothing invokes this command by default. It never writes to the forge, files issues, retries runs, quarantines, dispatches, posts upstream or schedules anything. See docs/ci-health.md for window, denominator, signature and coverage contracts.

```text
agent-board ci-health [OPTIONS] --config <FILE>
```

Options:

- `--config <FILE>`: Trusted JSON with explicit targets and an optional reviewed classification index
- `--until <UNTIL>`: Exclusive UTC date YYYY-MM-DD; defaults to today (30 complete UTC days)
- `--format <FORMAT>`: Stable JSON report or a concise human summary with coverage warnings (possible values: `json`, `human`) (default: `json`)

```text
Examples:
  Print the rolling report ending at today's UTC midnight:
    agent-board ci-health --config ci-health.json

  Print a human summary for a fixed exclusive end date:
    agent-board ci-health --config ci-health.json --until 2026-10-28 --format human

  Configuration shape (save as ci-health.json in the trusted checkout):
    {"targets":[{"repository":"cgwalters-forge/agent-board","workflow":"ci.yml"}],"index":{}}

  Configure 1 to 20 unique repository/workflow targets. Unknown fields are rejected.
  The optional index maps a prior report's 64-hex signature to a reviewed
  classification; observations never assign authoritative classes.
  Config field contracts: crates/board-forge/src/ci_health.rs (Config, Target,
  Classification). Both examples read GitHub and require a read token;
  neither publishes a report.
```

## agent-board prune

Open issues of the board's work repositories that are candidates to close (overtaken, duplicate, stale or triaged done), each with its reasons and evidence, unless labelled keep (read-only; docs/prune.md)

```text
agent-board prune [OPTIONS] --config <FILE>
```

Options:

- `--config <FILE>`: The pass configuration (docs/pass.md), from a trusted default-branch checkout: its `workRepositories` are read, `board` gives each issue's Priority and `bot` names the bot whose activity does not count. GH_TOKEN reads; nothing is written to the forge
- `--stale-days <DAYS>`: Days without activity by a person or a pull request after which an issue at neither P0 nor P1 is stale (1 to 365) (default: `21`)
- `--cache <FILE>`: The response cache: read if it exists, so unchanged pages are revalidated and cost no rate budget, then replaced by the responses this read used
- `--format <FORMAT>`: Output format (possible values: `text`, `json`) (default: `text`)

## agent-board promote

Preview operator /promote command evidence on configured forks, or with promote.onApproval the operator's current-head approval of a draft; standalone, not a pass stage. Policy and human-text provenance must pass. With promote.publish=true, --apply and --max-writes, revalidate all gates and the operator's current-head approval, rebase cleanly and add operator sign-offs where needed, then create an upstream draft or update an existing same-head PR with approved fork title/body, appending a closing reference for the source issue's optional Upstream: #N link. At most one candidate and four writes; one fork status comment and a create-only publication branch, no approved-ref updates, readiness changes or fork closure. Conflicting rebases hold. Bot-fork acceptance still needs recorded staging verification (docs/architecture.md#promote)

```text
agent-board promote [OPTIONS] --config <FILE>
```

Options:

- `--apply`: Publish at most one approved candidate; also requires promote.publish
- `--max-writes <MAX_WRITES>`: Whole-step publication cap (three writes, or four with a rebase/sign-off branch)
- `--config <FILE>`: Trusted shared pass configuration. Promotion discovers midstream forks from origin=fork and bot-midstream metadata; legacy promote.forks remains readable for one release as checked parent/base hints. Without promote settings or any discovery scope, off with no token lookup. Reads use the pass's request/time bounds
- `--record <FILE>`: Save live read responses projected to the evidence fields used here for offline replay. No forge writes. Record public repositories only before publishing the file; comment bodies remain in it
- `--replay <FILE>`: Preview recorded reads at their recorded clock, without credentials

## agent-board supervise

The controller's supervisor: a read-only health pass, whether its agent runs, and the executor of its dispatch requests

```text
agent-board supervise [OPTIONS] <COMMAND>
```

Commands:

- [`previous`](#agent-board-supervise-previous): Restore a verified previous report directory (read-only; GH_TOKEN Actions reads; explicit opt-in)
- [`health`](#agent-board-supervise-health): Read the health observations and write the report (read-only; GH_TOKEN reads)
- [`trigger`](#agent-board-supervise-trigger): Say whether the agent runs on this pass and print its inputs for $GITHUB_OUTPUT
- [`execute`](#agent-board-supervise-execute): Perform the agent's validated dispatch requests and log them (writes with GH_TOKEN, the bot's)
- [`due`](#agent-board-supervise-due): Say whether this pass is a sweep, from the previous run's times, for $GITHUB_OUTPUT (offline)
- [`repeat`](#agent-board-supervise-repeat): Say whether to skip the trigger because no check newly fails, for $GITHUB_OUTPUT (offline)
- [`compose`](#agent-board-supervise-compose): Compose the agent's task from the resolved profile, the trigger's task and the operator request (GH_TOKEN reads)
- [`preview`](#agent-board-supervise-preview): Print the agent's result comment without applying it (GH_TOKEN reads)
- [`session`](#agent-board-supervise-session): Print an operator session's wrapper inputs for $GITHUB_OUTPUT (GH_TOKEN reads)
- [`restart`](#agent-board-supervise-restart): Start the poll workflow once when its chain has stopped, and tell the operator once if that does not take; off unless the supervisor's file sets restartPass (writes with GH_TOKEN, the bot's: actions on dispatchRepo, comments on the log issue)

## agent-board supervise previous

Restore a verified previous report directory (read-only; GH_TOKEN Actions reads; explicit opt-in)

```text
agent-board supervise previous [OPTIONS] --repository <REPOSITORY> --workflow <WORKFLOW> --artifact <ARTIFACT> --branch <BRANCH> --event <EVENT> --destination <DIR>
```

Options:

- `--repository <REPOSITORY>`: Repository OWNER/REPO holding the artifact and its run
- `--workflow <WORKFLOW>`: Full workflow path, such as .github/workflows/health.yml
- `--artifact <ARTIFACT>`: Exact artifact name
- `--branch <BRANCH>`: Expected default branch (without refs/heads/)
- `--event <EVENT>`: Allowed run event; repeat for each event (at least one required)
- `--destination <DIR>`: Fresh output directory; parent must already exist

## agent-board supervise health

Read the health observations and write the report (read-only; GH_TOKEN reads)

```text
agent-board supervise health [OPTIONS] --config <FILE> --supervisor-config <FILE>
```

Options:

- `--config <FILE>`: The poll's agent-board.json: operator, bot, dispatch repository and workflow, follow
- `--supervisor-config <FILE>`: The supervisor's settings: workflows, thresholds, the release and wrapper to compare, the agent
- `--usage-config <FILE>`: Where the usage snapshot is ({repo, issue, bot}); without it the pace check is unavailable
- `--previous <FILE>`: The previous pass's report, for how long an observation has been unavailable
- `--out <FILE>`: Where the report is written (default: `health.json`)

## agent-board supervise trigger

Say whether the agent runs on this pass and print its inputs for $GITHUB_OUTPUT

```text
agent-board supervise trigger [OPTIONS] --config <FILE> --supervisor-config <FILE> --pools <FILE>
```

Options:

- `--config <FILE>`: The poll's agent-board.json: operator, bot, dispatch repository and workflow, follow
- `--supervisor-config <FILE>`: The supervisor's settings: workflows, thresholds, the release and wrapper to compare, the agent
- `--usage-config <FILE>`: Where the usage snapshot is ({repo, issue, bot}); without it the pace check is unavailable
- `--pools <FILE>`: The pools file naming each pool's agent, model and budget
- `--health <FILE>`: The report `health` wrote (default: `health.json`)
- `--daily`: The daily pass, which runs the agent with every check passing
- `--ledger <FILE>`: Previous executor ledger for advisory daily usage; omit when using the log path

## agent-board supervise execute

Perform the agent's validated dispatch requests and log them (writes with GH_TOKEN, the bot's)

```text
agent-board supervise execute [OPTIONS] --config <FILE> --supervisor-config <FILE> --pools <FILE> <--result-url <URL>|--requests <FILE>>
```

Options:

- `--config <FILE>`: The poll's agent-board.json: operator, bot, dispatch repository and workflow, follow
- `--supervisor-config <FILE>`: The supervisor's settings: workflows, thresholds, the release and wrapper to compare, the agent
- `--usage-config <FILE>`: Where the usage snapshot is ({repo, issue, bot}); without it the pace check is unavailable
- `--pools <FILE>`: The pools file; a request must name one of its pools
- `--profile-commit <COMMIT>`: The full commit printed by compose as profile_commit; required with a branch profile. Pass the trusted step output, not agent text
- `--result-url <URL>`: The agent run's result URL: its comment on the requests issue
- `--requests <FILE>`: The agent run's safe outputs (JSON Lines), handed back as an artifact of this run
- `--ledger <FILE>`: The previous executor run's ledger, an artifact of that run; leave it out when none was found
- `--ledger-run <ID>`: The id of the run whose artifact --ledger is
- `--ledger-out <FILE>`: Where this run's ledger is written for the next run; with --requests, the day's count comes from it, not the log issue

## agent-board supervise due

Say whether this pass is a sweep, from the previous run's times, for $GITHUB_OUTPUT (offline)

```text
agent-board supervise due [OPTIONS] --event <EVENT>
```

Options:

- `--event <EVENT>`: The event that started the run (`github.event_name`)
- `--prompt <PROMPT>`: The dispatch's prompt; one makes a prompt-less dispatch an explicit request
- `--previous <FILE>`: The previous run's times. Give it whenever the lookup worked, even if there was no previous run; left out, the times are unknown and only an explicit request is a sweep
- `--out <FILE>`: Where this run's times are written, for the next run to read (default: `times.json`)

## agent-board supervise repeat

Say whether to skip the trigger because no check newly fails, for $GITHUB_OUTPUT (offline)

```text
agent-board supervise repeat [OPTIONS] --config <FILE> --supervisor-config <FILE>
```

Options:

- `--config <FILE>`: The poll's agent-board.json: operator, bot, dispatch repository and workflow, follow
- `--supervisor-config <FILE>`: The supervisor's settings: workflows, thresholds, the release and wrapper to compare, the agent
- `--usage-config <FILE>`: Where the usage snapshot is ({repo, issue, bot}); without it the pace check is unavailable
- `--health <FILE>`: The report `health` wrote (default: `health.json`)
- `--previous <FILE>`: The previous run's report
- `--daily`: A sweep, which never skips

## agent-board supervise compose

Compose the agent's task from the resolved profile, the trigger's task and the operator request (GH_TOKEN reads)

```text
agent-board supervise compose [OPTIONS] --config <FILE> --supervisor-config <FILE> --task <TASK> --event <FILE>
```

Options:

- `--config <FILE>`: The poll's agent-board.json: operator, bot, dispatch repository and workflow, follow
- `--supervisor-config <FILE>`: The supervisor's settings: workflows, thresholds, the release and wrapper to compare, the agent
- `--usage-config <FILE>`: Where the usage snapshot is ({repo, issue, bot}); without it the pace check is unavailable
- `--task <TASK>`: The task `trigger` printed
- `--prompt <PROMPT>`: The dispatch's prompt; an operator comment's body takes its place
- `--event <FILE>`: The event payload (`$GITHUB_EVENT_PATH`)

## agent-board supervise preview

Print the agent's result comment without applying it (GH_TOKEN reads)

```text
agent-board supervise preview [OPTIONS] --config <FILE> --supervisor-config <FILE> <--result-url <URL>|--requests <FILE>>
```

Options:

- `--config <FILE>`: The poll's agent-board.json: operator, bot, dispatch repository and workflow, follow
- `--supervisor-config <FILE>`: The supervisor's settings: workflows, thresholds, the release and wrapper to compare, the agent
- `--usage-config <FILE>`: Where the usage snapshot is ({repo, issue, bot}); without it the pace check is unavailable
- `--result-url <URL>`: The agent run's result URL: its comment on the requests issue
- `--requests <FILE>`: The agent run's safe outputs (JSON Lines), handed back as an artifact of this run

## agent-board supervise session

Print an operator session's wrapper inputs for $GITHUB_OUTPUT (GH_TOKEN reads)

```text
agent-board supervise session [OPTIONS] --config <FILE> --supervisor-config <FILE> --session <FILE> --rules <FILE> --pools <FILE> --pool <POOL>
```

Options:

- `--config <FILE>`: The poll's agent-board.json: operator, bot, dispatch repository and workflow, follow
- `--supervisor-config <FILE>`: The supervisor's settings: workflows, thresholds, the release and wrapper to compare, the agent
- `--usage-config <FILE>`: Where the usage snapshot is ({repo, issue, bot}); without it the pace check is unavailable
- `--session <FILE>`: The session settings: enabled, repository, issue, idleMinutes
- `--rules <FILE>`: The session's own rules, put before the supervisor's profile
- `--pools <FILE>`: The pools file
- `--pool <POOL>`: The pool, of the pools file, the session runs on
- `--idle-minutes <IDLE_MINUTES>`: The idle timeout, 1 to 60 minutes; empty uses the session settings'
- `--prompt <PROMPT>`: The opening prompt, task data

## agent-board supervise restart

Start the poll workflow once when its chain has stopped, and tell the operator once if that does not take; off unless the supervisor's file sets restartPass (writes with GH_TOKEN, the bot's: actions on dispatchRepo, comments on the log issue)

```text
agent-board supervise restart [OPTIONS] --config <FILE> --supervisor-config <FILE>
```

Options:

- `--config <FILE>`: The poll's agent-board.json: operator, bot, dispatch repository and workflow, follow
- `--supervisor-config <FILE>`: The supervisor's settings: workflows, thresholds, the release and wrapper to compare, the agent
- `--usage-config <FILE>`: Where the usage snapshot is ({repo, issue, bot}); without it the pace check is unavailable

## agent-board status

How loaded the --owner organization is: runner jobs, agent runs, pools, merge queues and, with --project, the board (read-only; GH_TOKEN reads)

```text
agent-board status [OPTIONS] --config <FILE>
```

Options:

- `--config <FILE>`: Repository-owned agent-board.json from a trusted default-branch checkout: the dispatch repository and workflow whose runs are agent runs, the followed repositories whose queues are shown, and `pace`, where the usage snapshot is
- `--job-limit <N>`: Concurrent hosted-job limit to compare running jobs against

## agent-board release-train

One pass of the release train: cut a release and move its callers' pins, a step a pass (prints unless --apply)

```text
agent-board release-train [OPTIONS] --config <FILE>
```

Options:

- `--config <FILE>`: The train's configuration from a trusted default-branch checkout: the bot, the release repository, the wrappers and callers to pin, labels and thresholds
- `--apply`: Take the pass's step with GH_TOKEN, the bot's; without it the pass only reads and prints

## agent-board digest

Gather weekly public material and file or reuse its issue; preview unless --apply (docs/digest.md)

```text
agent-board digest [OPTIONS] --config <FILE>
```

Options:

- `--config <FILE>`: Trusted caller-owned JSON settings; see docs/digest.md
- `--partial`: Before Sunday 18:00 UTC, file the partial week up to now
- `--dry-run`: Print the body and intended writes without making them (the default)
- `--apply`: Create the issue, put it on the board and dispatch, with GH_TOKEN; DISPATCH_TOKEN starts the run if set

## agent-board broker

Probe the inference broker and keep one outage issue open while it is unreachable

```text
agent-board broker [OPTIONS] <COMMAND>
```

Commands:

- [`probe`](#agent-board-broker-probe): Probe the broker (TCP, then one HTTP request) and open, refresh or close the outage issue; prints unless --apply, and fails while the broker is unreachable

## agent-board broker probe

Probe the inference broker and keep one outage issue open while it is unreachable.

Run from a scheduled workflow that can reach the broker, for example every 15 minutes from the tailnet. GH_TOKEN (the bot's) is required even for preview: it reads the token's login and the open outage issues. Without --apply, each intended write is printed as 'would: ...'; with --apply, open or refresh an outage issue while unreachable, or close it and comment on recovery.

The probe tries TCP, then one unauthenticated HTTP GET, without redirects or an environment proxy. Each TCP connection attempt and the HTTP request have a 10-second timeout. Any HTTP response, including 401, 404, 500 or a redirect, counts as reachable. The first line is 'broker: reachable' or 'broker: unreachable'. An unreachable broker exits 1 after any writes; other errors also fail the command.

Read configuration from the caller's trusted default branch. All three JSON keys are required and unknown keys are refused: url is an HTTP(S) address without credentials; repository is OWNER/REPO for the outage issue; label is its label (created if missing), normally outage for poll's brokerOutage. Read the broker address from the deployment environment when preparing this file, rather than copying a production address into code.

```text
agent-board broker probe [OPTIONS] --config <FILE>
```

Options:

- `--config <FILE>`: The probe's configuration from a trusted default-branch checkout: the broker's url, and the outage issue's repository and label
- `--apply`: Open, refresh or close the outage issue with GH_TOKEN, the bot's; without it the probe reads and prints each write

```text
Examples:
  Preview using the caller's trusted configuration (still reads GitHub):
    agent-board broker probe --config broker.json

  Publish outage or recovery updates from the scheduled workflow:
    agent-board broker probe --config broker.json --apply

  Configuration shape (replace the example address for your deployment):
    {"url":"http://broker.example:8080/","repository":"OWNER/REPO","label":"outage"}
```

## agent-board apps

Create the per-role GitHub Apps (docs/apps.md)

```text
agent-board apps [OPTIONS] <COMMAND>
```

Commands:

- [`create`](#agent-board-apps-create): Register a role's GitHub App from the roles config through GitHub's manifest flow (one browser click)

## agent-board apps create

Register a role's GitHub App from the roles config through GitHub's manifest flow (one browser click)

```text
agent-board apps create [OPTIONS] --org <ORG> --role <ROLE>
```

Options:

- `--org <ORG>`: The organization that will own the app
- `--role <ROLE>`: The role in the roles config whose app this is
- `--config <FILE>`: The roles config (JSON); defaults to the roles compiled from this repository
- `--key-file <FILE>`: Write the new app's private key to FILE (created with mode 0600; an existing file is refused)
- `--key-stdout`: Print the new app's private key on stdout, and everything else on stderr
- `--key-stored`: The role's app exists and its private key is already stored: reuse it instead of creating one

## agent-board test

Run the credential-bearing staging integration test

```text
agent-board test [OPTIONS] <COMMAND>
```

Commands:

- [`review-setup`](#agent-board-test-review-setup): Check review wrapper variables in every workRepositories target; read-only, opt-in
- [`deploy`](#agent-board-test-deploy): Read-only staging deployment preflight, and with --apply the smoke path (writes); emits a JSON report
- [`project`](#agent-board-test-project): Create and clean up scratch objects in the staging project (writes)

## agent-board test review-setup

Check review wrapper variables in every workRepositories target; read-only, opt-in

```text
agent-board test review-setup [OPTIONS] --config <FILE>
```

Options:

- `--config <FILE>`: Trusted pass config; checks workRepositories and pr.reviewWorkflow

## agent-board test deploy

Read-only staging deployment preflight, and with --apply the smoke path (writes); emits a JSON report

```text
agent-board test deploy [OPTIONS] --config <FILE>
```

Options:

- `--config <FILE>`: Trusted default-branch deployment configuration (never dispatcher input)
- `--fixture <FILE>`: Offline JSON array of the four GraphQL responses; no credentials or network
- `--apply`: After a ready preflight, run the smoke path (writes): a fresh scratch repository and project, one fake-engine task, its result and derived Status, then cleanup by creation receipt

## agent-board test project

Create and clean up scratch objects in the staging project (writes)

```text
agent-board test project [OPTIONS]
```

Options:

- `--create-project`: Create and delete a throwaway project instead of using --project
- `--sweep`: Sweeping remains disabled: names and descriptions cannot authorize deletion
