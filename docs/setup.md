# Setup

> This text is LLM generated.

From installing the binary to a deployment that dispatches agent jobs from a
board. Bring a deployment up one lane at a time, with writes and scheduling off
until their previews and prerequisites are checked: a working offline rehearsal
does not establish a scheduled real-agent and review loop. The steps below use
GitHub; see [forges](forges.md) for Forgejo and GitLab.

## Install

Building agent-board needs Rust supporting edition 2024. The
[manual release path](release.md#manual-release-path) provides versioned Linux
x86_64 static binaries; [main CI artifacts](release.md#main-ci-artifacts)
provide the same assets for green main commits. It is not installable with
`gh extension install`.

```sh
cargo install --locked --git https://github.com/cgwalters-forge/agent-board agent-board
```

The binary is also built as `gh-agent-board`. Output follows `gh`: `--json`,
`--jq` and gh's exit codes. Live HTTPS requests use the platform trust store
through rustls-platform-verifier, not bundled Mozilla roots, so install a
sandbox egress proxy's or corporate CA certificate in the system trust store;
certificate and hostname verification stay on.

### Download and verify a release

Once the operator publishes a release, choose a reviewed tag and its full
40-character source commit from the repository, independently of downloaded
assets. Set `TAG` and `COMMIT` to those values, and work in an empty directory.
Use a recent GitHub CLI with `gh attestation verify` and `--source-digest`:

```sh
REPO=cgwalters-forge/agent-board
gh release download "$TAG" --repo "$REPO" --pattern '*-x86_64-unknown-linux-musl' --pattern SHA256SUMS --pattern release.json --pattern provenance.jsonl
GH_TOKEN='' GITHUB_TOKEN='' gh attestation verify release.json --bundle provenance.jsonl --repo "$REPO" \
  --cert-identity "https://github.com/$REPO/.github/workflows/release.yml@refs/heads/main" \
  --source-ref refs/heads/main --source-digest "$COMMIT" --deny-self-hosted-runners
jq -e --arg repo "$REPO" --arg tag "$TAG" --arg commit "$COMMIT" \
  '.schema == "agent-board.release.v1" and .repository == $repo and .tag == $tag and .commit == $commit and .target == "x86_64-unknown-linux-musl"' release.json
jq -rj '.sha256sums' release.json > attested-SHA256SUMS
cmp SHA256SUMS attested-SHA256SUMS
sha256sum --check attested-SHA256SUMS
```

Stop if any command fails. Checksums alone detect corruption, not authenticity:
the attested manifest and the independently selected commit bind these binaries
to the release workflow on main. Only after verification, install the binary
you need into a directory on `PATH`, for example
`install -m 755 agent-board-x86_64-unknown-linux-musl "$HOME/.local/bin/agent-board"`
(create that directory first). For the gh-style executable install
`gh-agent-board-x86_64-unknown-linux-musl` as `gh-agent-board`. Static linking
does not remove the need for the system CA store described above. Callers still
build from source until their own reviewed configuration adopts release downloads;
this workflow does not change tracker or enroll agent-board in the release train.

### Download and verify a main CI artifact

Select the latest successful **push to main** of `ci.yml`, not the latest run of
any workflow or a PR artifact. For rollback, optionally set `AGENT_BOARD_REV` to
a reviewed full 40-character SHA; otherwise leave it unset. In an empty directory,
use a recent GitHub CLI, jq and sha256sum. Run these commands with Bash's
`set -euo pipefail`. Select once and keep that run ID and head SHA together
throughout verification (do not resolve moving main again):

```sh
REPO=cgwalters-forge/agent-board
pin=()
if [ -n "${AGENT_BOARD_REV:-}" ]; then
  [[ "$AGENT_BOARD_REV" =~ ^[0-9a-f]{40}$ ]] || exit 1
  pin=(--commit "$AGENT_BOARD_REV")
fi
gh run list --repo "$REPO" --workflow ci.yml --branch main --event push --status success --limit 1 "${pin[@]}" --json databaseId,headSha > run.json
jq -e 'length == 1 and (.[0].headSha | test("^[0-9a-f]{40}$"))' run.json
```

Download only the verified bundle `agent-board-main`, not the intermediate
`agent-board` build artifact. The attestation prevents substitution of a binary
from another workflow, branch or commit, including untrusted PR builds:

```sh
RUN=$(jq -r '.[0].databaseId' run.json)
COMMIT=$(jq -r '.[0].headSha' run.json)
printf 'agent-board: run %s, SHA %s\n' "$RUN" "$COMMIT"
gh run download "$RUN" --repo "$REPO" --name agent-board-main
GH_TOKEN='' GITHUB_TOKEN='' gh attestation verify release.json --bundle provenance.jsonl --repo "$REPO" \
  --cert-identity "https://github.com/$REPO/.github/workflows/ci.yml@refs/heads/main" \
  --source-ref refs/heads/main --source-digest "$COMMIT" --deny-self-hosted-runners
jq -e --arg repo "$REPO" --arg commit "$COMMIT" \
  '.schema == "agent-board.release.v1" and .repository == $repo and .tag == "" and .commit == $commit and .target == "x86_64-unknown-linux-musl"' release.json
```

Only after the manifest verifies, check the downloaded binary digests:

```sh
jq -rj '.sha256sums' release.json > attested-SHA256SUMS
cmp SHA256SUMS attested-SHA256SUMS
sha256sum --check attested-SHA256SUMS
```

Stop on any failure; only then install as above. Callers need a token able to
read this repository's Actions artifacts; attestation verification itself uses
the included offline bundle with no token. A missing or expired artifact fails
the install: do not fall back to another SHA, an old `build-*` release, source
compilation or unverified bytes. The rollback SHA must have a retained successful
run using this artifact format.

## Development checks

Run recipes from the repository root with Rust supporting edition 2024. The
[Justfile](../Justfile) holds the commands and observed timings, not deadlines;
there is no Makefile or separate test harness. The [rust CI job](../.github/workflows/ci.yml)
runs `just check` followed by the commands in `just smoke`, in that order.
`just fmt` applies rustfmt; `just check` requires formatting and warning-free clippy
as well as the workspace tests. No current test requires root. The normal suite
uses fixtures and fake transports and is safe in a credential-free sandbox;
Cargo may fetch dependencies.

Separate CI jobs run `just lint`'s shared workflow check and the ignored
`forgejo_live` test (`just test-forgejo` locally; see [Live CI](forges.md#live-ci)).
Their checkout, container and environment prerequisites are noted beside the
recipes. The [site](../site/index.md) renders `docs/` in place, without copies,
under its own landing page; the README points to it. Its workflow runs
`just site-test`, `just site-build` and `just site-check`, then rebuilds on main
with `just site-build-pages` and uploads Pages. See the recipes for Node/lychee
requirements and the dependency-directory cleanup needed before workflow lint.

`just build-release` is the safe local build from the
[live workflow](../live/board-test.yml). Do not run that workflow's `test project`
invocation in a sandbox: it needs GitHub network access and a staging write token
and creates/modifies forge objects. PR CI's `rust` job has only `contents: read`,
no staging PAT, and disables persisted checkout credentials. The staging template
has `permissions: {}` and environment `Main`; only its `test` job's live invocation
receives `GH_TOKEN` from `secrets.PAT`, not its build step. Never execute unreviewed
code with that token. See the [CLI reference](cli.md) and
[implementation limits](design.md#current-implementation-limits).

## Try it offline

The fixtures are in the repository, not the installed package. Clone with
`git clone https://github.com/cgwalters-forge/agent-board`, then open that checkout's
root. To test that checkout rather than the latest upstream binary, install with
`cargo install --locked --path crates/agent-board`. Ensure Cargo's bin directory
(normally `~/.cargo/bin`) is on `PATH`. Installation needs network access for
source/dependencies; the commands below do not. Unset `GH_TOKEN`, `GITHUB_TOKEN`
and `DISPATCH_TOKEN` before this rehearsal; no `gh auth login`, model, broker,
container or board provisioning is needed.

```sh
agent-board item list --snapshot fixtures/board.json
agent-board reconcile plan --snapshot fixtures/board.json --emit
agent-board project view --snapshot fixtures/board.json --json project,coverage
agent-board pass --config fixtures/pass/config.json --replay fixtures/pass/board-2026-10-23.json --max-dispatch 1 --max-writes 3 --max-status-changes 5
```

All four should exit 0. Interpret them in order:

- `item list` lists the sample issues; it proves the binary can read a snapshot.
- `reconcile plan --emit` prints a JSON safe-output proposal (a Status update to
  `Triage`), not an applied repair. `project view` prints the project identity and
  coverage; complete coverage here is a fixture fact, not a live access check.
- `pass` starts with `pass: preview`. `--replay` supplies recorded API answers
  and a fixed clock, not the simpler `--snapshot` format. Do not add `--record`:
  that combines replay seeds with live reads. `--apply` conflicts with replay.
  The fixture config names historical repositories and enables simulated work;
  **do not copy it into a live caller** or drop `--replay` to try those identities.

The first pass includes `usage: no usage.url; pacing reads the published snapshot`
(the snapshot is also recorded), `follow: no repositories` (PR follow-up is not
exercised), `start triage … pool=claude` (would start, no model is invoked), and
`held by --max-dispatch: start …` (the cap deferred it). Other `hold …` lines
explain dependencies or operator work; they are not command failures. The last
two lines are:

```text
status: 0 change(s) of 0 item(s) read
summary: dispatched 1/1, writes 3/3, held 5, requests 57 (epics 17, schedule 37, status 3), endpoints {…}, causes {…}, points ~43, starts {}, comments created 0, edited 0, not ready {…}, planned starts {"triage":1}
```

These are **planned** dispatch/write steps, not completed mutations. `held` counts
cap-held steps, not every `hold` line; requests are replayed reads, counted by
the stage that read them, then by endpoint and by the candidate or epic they
were read for ([what is built](pass.md#what-is-built)), `~` marks
estimated GraphQL points, and `not ready` counts the candidates not started
by why. This recording does not exercise Status changes.
The caps limit the proposal; preview is what prevents writes. A `fail …` or
`… error: …` line means an operation failed; the pass exits nonzero after its
summary, and a partial preview is not an approval to apply.

The existing offline CLI test executes this exact command block in `just check`
in CI and checks the pass's decisions and summary. Nothing here proves live
token access, dispatch, inference, review, container isolation or a scheduled
loop. Every flag is in the [CLI reference](cli.md).

### Compare two revisions

A change to the pass can pass its fixtures and review and still do nothing on
the live caller: a day's run of fixes did (#887), because nobody had run the
changed pass against the caller's configuration before merging. The decision
diff previews the same recording with two checkouts' binaries, so the only
input that differs is the code:

```sh
node workflow/decision-diff.cjs BASE HEAD [CONFIG [RECORDING]]
```

For an implement run, create the base checkout with
`git worktree add ../base <start commit>`, then run `just replay` and
`just decision-diff ../base`. Changes to the pass, its configuration or fixtures
require this comparison before hand-back, in addition to `just check`. Where the
run has the token, preview the changed command against the caller configuration;
otherwise use the recorded live pass. Paste at least both summary lines in the
hand-back. A live-symptom fix must show the symptom change, or explicitly say that
the recording does not exercise it or lacks the head's required reads. A fresh
recording requires the bot's token (`pass --record`).

It builds agent-board in both checkouts (`cargo build --locked`), runs the
rehearsal above with each binary, with `GH_TOKEN`, `GITHUB_TOKEN` and
`DISPATCH_TOKEN` removed from its environment, and prints both summary lines
and the unified diff of the two previews: starts, holds, writes, not-ready
reasons, requests per stage and the summary itself, or `no change`. CONFIG and
RECORDING default to BASE's `fixtures/pass/config.json` and
`fixtures/pass/board-2026-10-23.json`. It exits 0 whenever both previews ran,
whatever they decided; a head whose replay fails is shown with its exit
status and last lines. Each build may take 20 minutes and each preview 60
seconds, and what is printed is cut at 400 diff lines, 16 KiB of diff and
4 KiB a line, so a head that hangs or prints without end is reported as cut
rather than stopping the job or losing the review comment to GitHub's size
limit.

The review job runs it on every pull request that touches the pass and pastes
the output in the review; an implement run does the same against its start
commit before handing back ([AGENTS.md](../AGENTS.md)). The diff
shows only what the recording exercises: the committed one is a preview of
2026-10-23 with that day's configuration copy. A head that reads something it
lacks fails replay, and a symptom the recording lacks cannot change in it. A
fresher recording is a live preview with the caller's configuration and the
bot's token, `agent-board pass --config pass.json --record FILE`; look at it
before committing it, since it keeps comment bodies
([recording and replay](forges.md#recording-and-replay)).

## This repository's runner maintenance

`.github/renovate.json` adopts the published
[runner-only preset](https://github.com/cgwalters-forge/actions/blob/main/renovate/README.md).
It follows the provider's **main branch**, not a pinned revision: future preset
changes reach this repository too. The preset disables other dependency updates
and vulnerability-alert PRs, enables only Ubuntu `github-runners` updates, and
does not automerge. Dependabot still owns action pins; its configuration is
unchanged. Adoption grants no workflow permission or credential. The Renovate
App must already be installed with access to this repository; a configuration
file alone does not prove that access or a live update.

The credential-free `.github/agentic-job/test_review.py` suite checks adoption
and each directly owned workflow job, including build/release workflows and
staging templates when present. CI migrated to `ubuntu-26.04` in #535; the
staging templates now use it too. Reusable workflows retain ownership of their
runners; do not copy their jobs or move privileged pins just to change labels.
Intentional special runners (for example the provider's detector and agent
runner, which needs systemd 257) must retain their separate requirements.

Runner ownership audit of this branch's pinned providers (2026-11-04):

- Review calls `cgwalters-forge/actions/.github/workflows/cgwalters-review.yml`
  at `d8e22f71b2abbe5f41306a0c7ef1b6c3d2829cac`. Its refusal, submit and status
  jobs use 26.04. Its agent uses `AGENT_RUNNER`, defaulting to 26.04. Its
  transitive `policy.yml`, `check.yml` and `apply.yml` calls belong to
  `cgwalters-forge/agentic-job@v0.6.2`
  (`258dc11ca7fe1798836976c0424274d1498e22cb`): policy defaults to 24.04,
  passed through policy outputs to check/apply; the detector explicitly uses
  26.04. The wrapper does not expose an ordinary-runner override. Migration
  needs a provider change and review of the privileged wrapper pin and identity
  admission; this adoption does not change them.
- Reconciliation calls `cgwalters-forge/agentic-job/.github/workflows/agentic-job.yml`
  at `db6f8e67b0855ba342c3f32a7083b4243a8ae995` (v0.6.3). Its local policy,
  check and apply workflows run at that same revision: ordinary `runner`
  defaults to 24.04, while `agent-runner` and the detector use 26.04. The
  proposals-only caller skips agent/registration. The caller now passes
  `with.runner: '"ubuntu-26.04"'` to migrate policy/check/apply without moving
  the provider pin or changing permissions, credentials or identity admission.
  The provider default remains older; its JSON input is manually maintained.

The merge target, main at `1984af5822eac67f1dfc67c374bf9554e7466fca`, has
already reviewed newer provider pins independently of this patch: review uses
the actions wrapper `1b4539dd00edff3f89cb130e72e535e8d9c6a67a`, calling
agentic-job v0.6.9 (`57119eeffa17ea41b383cfdaf8370b8abeba8868`). Its ordinary
policy default is 26.04, passed to check/apply; wrapper jobs and the detector
also use 26.04. Thus the old review-provider blocker above does not remain
after merging main. Main's reconcile pin is v0.6.4
(`732db8c24295ee8d4a79de19f588e6f871d35fe0`), which still defaults to 24.04
and accepts the same JSON override. This patch preserves both branches' pins
and lets their existing updates merge normally; it changes no identity admission.

The preset extracts literal `runs-on` strings/lists only. There are no owned
matrix/variable runner labels today. Provider `inputs.runner`, policy outputs,
`inputs.agent-runner` and review's `vars.AGENT_RUNNER` need manual maintenance;
the preset cannot follow them. Never mechanically rewrite self-hosted label
lists or specialized runners. The preset does not scan `live/`; staging templates
are manually maintained until copied into a staging repository's workflows.
The installation API returned 401 on 2026-11-04:
App access is unknown, not absent. Live app access and a Renovate-authored update
remain separate evidence under tracker#1205, not a prerequisite for merging this
configuration patch. See the adoption PR for the dated checks and hosted CI links.

## Connect one lane

A deployment is configuration on a caller repository's protected default
branch plus [workflow callers](#pinned-callers), each of which builds
agent-board at a pinned, reviewed revision and runs one subcommand. No
agent-board code is copied into the caller or resynced from another
deployment. Two steps below still copy workflow files from elsewhere, the
dispatch caller (step 2) and the review caller (step 4); each says so.

1. **Choose the identities.** A target repository, a trusted caller repository
   that holds the dispatch workflow, its bounds, agent-board's configuration
   and the callers, and an existing organization Project v2. Each is a
   separate identity; none is provisioned for you by an existing deployment.

2. **Prove model-free dispatch first.** This step still copies a workflow: the
   caller needs its own dispatch workflow, which calls a reusable agentic-job
   workflow (directly, or through an organization wrapper), with its bounds
   and the profile of each kind beside it. Follow agentic-job's
   [ten-minute scripted trial](https://github.com/cgwalters-forge/agentic-job/blob/main/docs/dispatch.md#ten-minute-scripted-trial):
   copy its dispatch workflow and bounds into the caller, pin the reusable
   workflow to a reviewed commit, set exact `repos` and keep `bases = ["main"]`.
   Commit to the protected default branch, allow the actions, then dispatch
   `kind=research` against an existing issue. Implement runs also need the
   apply environment and its token. Bounds protect documentation too: list
   the intended documentation files in `unprotected_files` rather than
   unprotecting the repository. The workflow must take the inputs and set the
   run title agent-board expects ([dispatch](dispatch.md)).

3. **Provision inference and network before selecting a real agent.** Follow
   agentic-job's [Switch to a real agent](https://github.com/cgwalters-forge/agentic-job/blob/main/docs/dispatch.md#switch-to-a-real-agent)
   and [broker admission](https://github.com/cgwalters-forge/agentic-job/blob/main/docs/inference.md#which-workflows-a-broker-admits),
   matching admission to the identity that calls the broker.

4. **Install review independently.** This step also still copies a workflow:
   each reviewed repository needs its own review caller and its bounds and
   task files. Follow agentic-job's review
   [Another repository](https://github.com/cgwalters-forge/agentic-job/blob/main/docs/review.md#another-repository)
   and [Configuration](https://github.com/cgwalters-forge/agentic-job/blob/main/docs/review.md#configuration);
   [actions#129](https://github.com/cgwalters-forge/actions/issues/129) plans
   one organization-wide review workflow instead.
   Choose merge policy separately: an agent verdict is not human authorization.

5. **Provision and read the board before writes.** Pre-create the Status
   options `Triage`, `Backlog`, `Todo`, `In Progress`, `Draft`, `In Review`,
   `Blocked`, `Needs human`, `Done` and `Cancelled`, and the Priority
   single-select with `P0`, `P1` and `P2`
   ([durable model](design.md#1-scope-and-durable-model); Status is derived by
   the pass, see [architecture](architecture.md#status)).
   `project init` creates only missing Turn, Run and Result fields, with the
   operator's token, and never edits existing fields or options;
   `project init --snapshot FILE` instead prints copyable
   `gh project field-create` commands on stderr without running them. For asks,
   provision the `question` and `needs-human` labels and an assignable
   operator. Start with `snapshot create` and `reconcile plan` previews. Live
   writes need a token with organization Projects read and write and repository Issues write, with
   organization approval and the token owner's access to the project; workflow
   `permissions` and `GITHUB_TOKEN` do not grant organization Projects writes.
   The [live template](../live/board-test.yml) shows a protected staging test.

6. **Preview the first live pass, then wire scheduling deliberately.** Put a
   separate `pass.json` in a trusted default-branch checkout of your caller.
   For a read-only board preview, replace every example identity below with your
   own (project is the positive organization project number, not its node ID):

   ```json
   {
     "operator": "your-operator",
     "bot": "your-bot",
     "dispatchRepo": "your-org/your-caller",
     "dispatchWorkflow": "dispatch.yml",
     "dispatchRef": "main",
     "board": {"owner": "your-org", "project": 1},
     "workRepositories": ["your-org/your-target"],
     "wip": 0,
     "triage": false
   }
   ```

   The real Config parser requires `operator`, `bot`, `dispatchRepo` and
   `dispatchWorkflow` even with work off. It rejects unknown keys and explicit
   nulls; omit unused sections. `wip: 0` disables new work, absent `follow`
   disables PR follow-up, and absent `epics` disables epic planning. The board
   section still lets the Status stage preview repairs. With a read-only
   `GH_TOKEN` for the bot (Projects read plus Issues/Pull requests read on the
   exact repositories it must inspect, and Actions read on `dispatchRepo` to
   list runs in flight even with `wip: 0`), from that checkout run:

   ```sh
   agent-board snapshot create --owner your-org --project 1 > board.json
   agent-board reconcile plan --snapshot board.json --emit
   agent-board pass --config pass.json --max-dispatch 0 --max-writes 0 --max-status-changes 5
   ```

   `agent-board pass --config pass.json --check-config` checks the file
   alone, as a pass would, with no token or forge read for explicit repository
   lists. Organization entries require bounded live resolution and a read token
   in a trusted context ([repository sources](pass.md#organization-repository-entries)); run the offline check in the
   caller's pull request CI with the pinned agent-board, so a change the
   pass refuses fails review rather than every pass after merge.
   No `--apply`, no `--record`, no scheduled caller and no dispatch token yet.
   Zero caps also bound proposals; Status changes over five refuse the stage
   whole rather than silently applying a subset. Inspect errors and held lines,
   not just exit 0. A live preview proves reads and planning, not writer access
   or dispatch. To preview candidates later, `wip` must be positive and
   `workRepositories` exact; automatic runs need `pools` and `usage.repository`
   plus `usage.issue`. Refreshing usage additionally needs `usage.url`; without
   it the pass reads the published snapshot. Triage needs `triage` and
   `triagePools` or `planPools` naming configured pools; scope `epics` also needs
   `epics`. Keep all optional features absent until their prerequisites are met.
   See the [configuration reference](pass.md#one-configuration-file); the setup
   CLI test also parses the minimal example above with the real Config parser.

   Use
   [job-plan dispatch](job-plans.md#dispatch), starting with
   `dispatch ISSUE --dry-run`; apply needs a writer's user token and records the
   plan before dispatching. Write the callers' configuration on the caller's
   protected default branch: owner and project, exact repository lists, the
   dispatch caller and the identities, never from dispatcher-set inputs. Every
   option ships off; [pull request follow-up](poll.md#configuration) and the
   [pass configuration](pass.md#one-configuration-file) list them. Add the
   [callers](#pinned-callers) one at a time, each previewing before it
   applies. Audit dispatch and fix bounds and pools before enabling a scheduled
   applier, and never run two appliers against one board.

Only the operator can do the following; no agent or guide can do it: authorize the network
identity and its ACLs, admit the specific repository and workflow identities and
finite budgets at the broker, keep model credentials only at the broker, and
provision the protected apply, reconcile and live-test secrets with effective
Projects access.

Before any live dispatch, the operator must also approve the organization's
Actions/reusable-workflow policy, the bot's repository/project access and any
token SSO approval. The dispatch credential needs Actions write in the caller;
`DISPATCH_TOKEN` can separate that identity from `GH_TOKEN` (otherwise the pass
uses `GH_TOKEN`). Applying passes need the configured bot's effective write
access described in step 5; PR follow-up/merges and agentic-job apply have their
own repository permissions and protected environments. A read token that can
preview is not evidence that any of those writers are provisioned. Review callers,
broker admission, network ACLs and protected secrets are separate installations,
not side effects of `cargo install` or `project init`.

### Pinned callers

Each caller is a scheduled or dispatched workflow in the caller repository.
It checks out the caller's default branch for its configuration, builds
agent-board at a full commit reviewed in the caller repository (never a
branch, since the binary then runs with a write token; the build step itself
gets no token), and runs one subcommand, for example:

```sh
[[ $AGENT_BOARD_REV =~ ^[0-9a-f]{40}$ ]]
cargo install --locked --root "$RUNNER_TEMP/agent-board" \
  --git https://github.com/cgwalters-forge/agent-board --rev "$AGENT_BOARD_REV" agent-board
"$RUNNER_TEMP/agent-board/bin/agent-board" pass --config CONFIG
```

Moving a caller to a newer agent-board is a reviewed edit of its pinned
revision. Configuration is read only from files on the default branch; a
caller may add a value from a repository variable, such as the broker's
address, before passing the file on. The caller workflows themselves are
written per deployment: copying another deployment's means replacing its
names, secrets and defaults, and resyncing the steps around each subcommand
by hand.

> **Example caller, not a requirement:** cgwalters-forge/tracker at
> [`bb8363a`][tracker]. The paths and file names are that deployment's.
> [tracker#890](https://github.com/cgwalters-forge/tracker/issues/890) plans
> to move the deployment to cgwalters-forge/infra, so the caller repository
> may change.
>
> - [`poll.yml`][poll], every ten minutes: `agent-board pass --config .github/pass.json`
>   ([pass](pass.md)) with `--max-dispatch`, `--max-writes` and
>   `--max-status-changes` caps; a scheduled pass adds `--apply` only once a
>   repository variable enables it. After an applying pass its `next` job
>   starts the next one ten minutes after this one's start, and wakes the
>   supervisor when it has slept 30 minutes, in shell at that revision;
>   [`agent-board pass next`](pass.md#the-next-pass) is that job as one
>   invocation, such as `agent-board pass next --workflow poll.yml --branch main
>   --input apply=true --apply-title 'pass (apply)' --wait-minutes 10
>   --supervisor-workflow supervisor.yml --supervisor-stale-minutes 30
>   --supervisor-input prompt= --supervisor-input session=false`.
> - [`release-train.yml`][release-train], hourly: `agent-board release-train --config .github/release-train/config.json`
>   ([release train](release-train.md)), off until a repository variable
>   enables it.
> - [`supervisor.yml`][supervisor], on its own crons, by dispatch and on the
>   operator's comments on its requests issue, with this deployment's
>   defaults for the operator and that issue:
>   `agent-board supervise` `due`, `health`, `repeat`, `trigger`, `compose`,
>   `preview` and `execute` ([supervisor](supervisor-requests.md)), reading
>   `.github/poll/agent-board.json`, `.github/supervisor/config.json`,
>   `.github/usage/config.json` and `.github/agentic-job/pools.json`; its
>   agent job runs the dispatch wrapper with `.github/agentic-job/supervisor.toml`.
> - [`session.yml`][session], by dispatch: `agent-board supervise session`,
>   reading `.github/poll/agent-board.json`, `.github/supervisor/config.json`,
>   `.github/agentic-job/session.json`, `.github/agentic-job/profiles/session.md`
>   and `.github/agentic-job/pools.json`; its agent job runs the dispatch
>   wrapper with `.github/agentic-job/dispatch.toml` and `hosted.toml`.
> - [`broker-probe.yml`][broker-probe], every 15 minutes: `agent-board broker probe --config .github/broker/config.json --apply`
>   ([broker probe](broker-probe.md)), with the broker's address added, off
>   until a repository variable enables it.
>
> Two parts of that caller are still copies rather than configuration:
> [`dispatch.yml`][dispatch], which calls cgwalters-forge/actions'
> `cgwalters-agentic-job.yml` wrapper, with its bounds and profiles under
> `.github/agentic-job` (step 2), and [`review.yml`][review] with
> `workflow/review.*` (step 4). Its callers also build through a
> `just build-agent-board` recipe in tracker's own Justfile, which runs the
> check and `cargo install` above (taking the source from `agent-board.json`);
> a deployment does not need that recipe.

[tracker]: https://github.com/cgwalters-forge/tracker/tree/bb8363a8cc856cbdb5538b1089d3d175385a3a91
[poll]: https://github.com/cgwalters-forge/tracker/blob/bb8363a8cc856cbdb5538b1089d3d175385a3a91/.github/workflows/poll.yml
[release-train]: https://github.com/cgwalters-forge/tracker/blob/bb8363a8cc856cbdb5538b1089d3d175385a3a91/.github/workflows/release-train.yml
[supervisor]: https://github.com/cgwalters-forge/tracker/blob/bb8363a8cc856cbdb5538b1089d3d175385a3a91/.github/workflows/supervisor.yml
[session]: https://github.com/cgwalters-forge/tracker/blob/bb8363a8cc856cbdb5538b1089d3d175385a3a91/.github/workflows/session.yml
[broker-probe]: https://github.com/cgwalters-forge/tracker/blob/bb8363a8cc856cbdb5538b1089d3d175385a3a91/.github/workflows/broker-probe.yml
[dispatch]: https://github.com/cgwalters-forge/tracker/blob/bb8363a8cc856cbdb5538b1089d3d175385a3a91/.github/workflows/dispatch.yml
[review]: https://github.com/cgwalters-forge/tracker/blob/bb8363a8cc856cbdb5538b1089d3d175385a3a91/.github/workflows/review.yml

## Repeat a staging trial

Before provisioning, preview the deployment prerequisites with the opt-in,
read-only test runner (from the checkout root):

```sh
agent-board test deploy --config fixtures/deploy-config.json --fixture fixtures/deploy-reads.json
```

This offline example exits 0, reports simulated repository/project/workflow and
source-pin reads as `ready`, and still marks environment provisioning, effective
credential capabilities and inference/broker admission `unverified`. Fixture pins
and node IDs are invented; never use them live. A report is not deployment
authorization, and exit 0 does not mean all prerequisites are ready.

For live reads, copy that config to a trusted default-branch file, replace every
identity and both full lowercase 40-hex commit pins with reviewed values, then
omit `--fixture`. The workflow is a basename in that same staging repository's
`.github/workflows/`; `projectId` and `project` must both match the organization
project. The source pin is in `cgwalters-forge/agent-board`. The command rejects
owners other than `cgwalters-forge-stage`, repository owner mismatches, unknown
keys, nulls and moving refs before requesting anything. This stage-only restriction
prevents a rehearsal from targeting production; it cannot tell whether an operator
has put production data in the staging organization.

The `agent-board.deploy-preflight.v1` JSON report has `mode: "preview"`, `writes: 0`
and ordered `checks`, each with `group`, `check`, `state` (`ready`, `blocked` or
`unverified`) and a non-secret `detail`. Configuration failures and mismatched or
missing returned facts and unreadable, malformed or wrongly sized fixture files
are blocked (exit 1); failed live reads are unverified, never ready. Project
numbers must fit a positive GraphQL Int (at most 2147483647). The report is always
printed in full; global `--json`, `--jq` and `--board-config` are unsupported.
It attempts four logical non-paginated GraphQL queries (the normal client's
bounded transport retries may make additional requests), uses `GH_TOKEN` via
the normal client only without `--fixture`, and without `--apply` (below) has no
mutation path. Environment provisioning is reported separately from stage access and from
inference/broker prerequisites; it reads no secret values and does not test writer
tokens by mutation. Actions policy, workflow contents/transitive pins, protected
environment restrictions and effective token permissions require operator review.
No resources, secrets, workflows or schedules are installed or enabled. The
unanswered operator decisions remain in
[#282](https://github.com/cgwalters-forge/agent-board/issues/282), not in this report.

**Unverified:** this walkthrough's offline rehearsal is not the live staging
trial. Live provisioning, model-free workflow dispatch, inference, review,
protected apply and scheduled operation remain unattempted here; satisfy the
operator prerequisites above (the work tracked by
[#282](https://github.com/cgwalters-forge/agent-board/issues/282)) before trying
them independently.

With the prerequisites satisfied, use a disposable staging repository and
project selected by the trusted configuration; do not reuse a production board
or enable a scheduled applier. Use at most three small, independent issues: one
research task and two harmless documentation tasks. Add them with `item add`
previews before applying each addition. For each, have an accepted writer post a
plan, inspect `dispatch --dry-run`, then dispatch it once. Stop at the first
failed prerequisite; do not work around bounds, widen token permissions blindly,
or replace an ambiguous dispatch record to force a retry. Keep batching and
scheduled dispatch off so each result can be attributed to its plan.

Inspect the dispatch record and the linked run, then preview the matching
Run/Result update with `run record` before applying it. Verify any pull request
and exact-head review separately: neither a successful dispatch nor a recorded
run proves the task succeeded.

Report the date, workflow revision, non-secret configuration names, issue, plan,
run and pull request links, exact commands and exit statuses, expected versus
actual results, and every guess or workaround. Mark unattempted steps
unverified, and remove only scratch objects the trial created.

### The stage deploy smoke path

`test deploy --apply` runs the same preflight live (it refuses `--fixture`), and
writes only if all four reads are `ready`. It then runs one fixed sequence in
the staging organization, each step checked against what GitHub returns:

1. creates a fresh private scratch repository and a Project v2 linked to it,
   both named by a random `agent-board-smoke-…` nonce;
2. creates one issue there and adds it at Status `Todo`;
3. runs the pass's reconciliation over a complete snapshot of the new project,
   which must propose nothing;
4. dispatches one `research` task through the normal dispatch interface to an
   in-process fake engine, which checks the exact `workflow_dispatch` request
   for the configured workflow and answers a run ID, sending nothing and running
   no model;
5. performs the fake engine's harmless result (one comment, then closing the issue
   as completed) and reads both back exactly;
6. has reconciliation derive `Done` from the closed issue, writes it, and reads
   it back.

At most 8 writes come before cleanup. A 10-minute deadline is checked before
each step and also bounds rate-limit waits. Cleanup always follows and acts only
through creation receipts: it deletes the project by its returned ID, then
deletes the repository by name only after reading that name back from the
returned node ID. A creation whose returned owner or name is not the one
requested is never cleaned up, and neither is one whose answer was lost: both
are reported as leftovers, the latter by the nonce name to look for. Nothing is
ever deleted by prefix, or by a name not read back from a receipt. Receipts go to stderr as they are made, so a run
killed outright still leaves its IDs in the log. The threats these bounds stop
are stage credentials reaching production and deletion of resources this run
did not create.

#### Opt-in model-free Actions task

The threat is executing arbitrary caller or task code with stage administration
credentials. The optional `"actions": true` in the trusted deploy JSON instead
installs only the compiled-in [`stage-smoke-task.yml`](../live/stage-smoke-task.yml)
in the fresh, private scratch repository. It dispatches that workflow once using
the normal dispatch request. The job checks out no code, uses no external action,
receives no stage token and runs no agent or model: only its own repository's
`GITHUB_TOKEN`, with Issues write, comments on and closes the scratch issue. Input
values go through environment variables, not shell interpolation. Omitted or
`false` keeps the existing fake engine; `--apply` and ready preflight reads are
still required. A fixture or preflight never installs or dispatches a workflow.

This mode requires Contents and Workflows write to install the workflow, Actions
write to dispatch/read its run, and organization Actions policy admitting a
GitHub-hosted runner. These are additional stage-only capabilities, not production
reporting authority. Nothing provisions credentials or changes organization
policy. Keep the enclosing `stage-deploy` environment default-branch restricted;
reports remain artifacts in the stage repository, not production issue writes.

The runner reserves at most 10 writes before cleanup, including the workflow's
two issue writes even if dispatch fails ambiguously. It refuses to retry a
dispatch, including the dispatch interface's compatibility fallback, so it
first reads the installed workflow at most 12 times, five seconds apart, until
Actions reports it registered and active. It polls the returned run ID at most
40 times, five seconds apart, checking the overall deadline before each read;
a run not yet readable counts against those polls. The repository, installed commit, workflow path and
dispatch event must match, and the run must finish successfully. Only then does
it independently read the exact result comment and completed issue; it never
substitutes local result writes. It records the run ID and installed commit in
the report before waiting. Cleanup still follows failure or cancellation using
the original creation receipts. A killed process can still leave resources;
use the logged receipts, not prefix sweeping.

This is an Actions-backed **task**, not an installed pass scheduler or the
agentic-job dispatch/review/apply system. The report explicitly records that
limitation and the README setup's copied dispatch/review caller workflows.
No green live report or installed recurrence is claimed. For #282 the reasonable
engineering choice is model-free execution and stage-local artifact reporting;
inference admission/budget and production reporting are not needed for this slice.

The `agent-board.deploy-smoke.v1` report adds `writes` (cleanup included) and a
`smoke` object with `steps` (`ok`, `failed` or `cancelled`), `receipts`, `cleanup`
(`deleted` or `failed`) and `leftovers`. Remove leftovers by hand using
the IDs or names in the report. Exit status 0 means every step passed and nothing
was left, 2 means the deadline stopped the run (cleanup still ran), 4 means
GitHub refused the credentials, and 1 means any other failure, a refused
preflight included. Errors go to stderr only.
[`live/stage-deploy.yml`](../live/stage-deploy.yml) is the staging-side template:
manual, weekly and release dispatch from the default branch only, environment
`stage-deploy`, and the report uploaded as an artifact. agent-board is built at
the trusted config's `agentBoardRevision`, which must already be on
agent-board's `main`, so a pull request's head is refused even by SHA. A release
dispatch's `release` input must equal that pin; approving a release means
reviewing the config change that moves the pin. `STAGE_DEPLOY_TOKEN` is passed
only to the run step. Live mutation stays off until `STAGE_DEPLOY_APPLY` is `true`; without
it the template runs the read-only preflight.

None of this is installed or has run live. The remaining steps, after #282's
decisions:

1. Choose the staging repository (owner `cgwalters-forge-stage`) and commit a
   trusted `.github/agent-board/stage-deploy.json` to its protected default
   branch, in the shape of [the fixture](../fixtures/deploy-config.json), with
    reviewed values. Its `workflow` is checked to exist and named to the default
    fake engine: `stage-deploy.yml` itself will do. With `actions: true`, the
    task workflow is instead installed in the scratch repository as described
    above; the configured workflow remains a preflight prerequisite.
2. Copy `live/stage-deploy.yml` to that repository's `.github/workflows/`, and
   allow the pinned `actions/checkout` and `actions/upload-artifact`.
3. Create environment `stage-deploy` with a deployment branch restriction to the
   default branch. Required reviewers would make each weekly run wait for
   approval; whether to have them is one of #282's decisions. Give it secret `STAGE_DEPLOY_TOKEN`:
   a fine-grained token with resource owner `cgwalters-forge-stage`, all
   repositories (it must delete the repositories it creates), Administration,
   Issues and Metadata access for repositories, and Projects read and write for
   the organization. Also allow members to create and delete repositories.
4. Dispatch it once without `STAGE_DEPLOY_APPLY` and check that the preflight
   report's reads are `ready`.
5. Set environment variable `STAGE_DEPLOY_APPLY=true`, dispatch once, and confirm
   the artifact has every step `ok`, both cleanups `deleted` and no leftovers;
   check the organization for anything named by the report's nonce.
6. Only then rely on the weekly schedule, and on release-time dispatch with
   `release` set to the commit the trusted config was updated to.

## Scheduled reconciliation

This repository's [reconcile.yml](../.github/workflows/reconcile.yml) is a
staging example of applying `reconcile plan` repairs from CI; the pass's
Status stage ([pass](pass.md#order-and-effects)) replaces it at cutover, so a
deployment does not copy it: once its pass caller runs, that derives Status.
Every ten minutes on main it reads a staging board with a read-only token,
writes `reconcile plan --emit` proposals as an artifact, and hands them to
agentic-job's independent check and apply, bounded by [reconcile.toml](../.github/agentic-job/reconcile.toml) to
Status updates on one project and issue closes in one repository. The board
comes from the repository variables `RECONCILE_OWNER`, `RECONCILE_PROJECT` and
`RECONCILE_REPO`; until they and the read token are set, each run skips with a
notice. Apply is skipped when nothing is proposed, and it does not reread the
board, so an issue reopened between snapshot and apply can still be set Done.

It needs two environments:

- `staging-project-read` holds `STAGING_PROJECT_READ_TOKEN`, with organization
  Projects **Read** and repository Issues **Read** on the staging repository
  (plus GitHub's required Metadata read), and no write credential. Set its
  deployment branches and tags to **Selected branches and tags**, with
  only a **branch** rule for `main` and no tag rules. Do not use "Protected
  branches only", which can admit other protected branches.
- `staging-project-apply`, limited to main the same way, holds
  `SAFE_OUTPUTS_PAT` with organization Projects read and write and Issues write
  on the bounds' repository.

Remove any repository or organization secret of the same names accessible to
this repository, so a branch workflow cannot bypass an environment by omitting
`environment`; revoke and reissue a token that was ever a repository secret.
Keep changes to these environments restricted to trusted administrators.
