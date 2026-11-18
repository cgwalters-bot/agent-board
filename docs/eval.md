# Three-arm role evaluation

> This text is LLM generated.

This is an offline protocol, not a launcher. Nothing runs by default and no
workflow, provider routing, broker or installed agent changes. The concrete
threat to validity is comparing different tasks, prompts, prices or environments
and crediting the arm for the difference. One mechanism addresses it: freeze a
manifest and reject records that do not match it. Validation is bounded to 54
records, two or three tasks, and 4 MiB per CLI input. These are validity checks,
not a sandbox for executing untrusted historical code.

The protocol, its pending manifest and preflight report come from
[agenthome](https://github.com/cgwalters-forge/agenthome/tree/ce75a397b285ee000d4046bae405e0ecb6bb08d6/eval),
where the evaluation was planned
([agenthome#52](https://github.com/cgwalters-forge/agenthome/issues/52)); the
role prompts and model configuration it compares stay there.

## The command

```text
agent-board eval summarize MANIFEST.json RUNS.json
```

It reads a manifest and an array of run records, validates both against the rules
below and prints the summary on stdout as `JSON.stringify(summary, null, 2)`
would, exiting 1 with the reason on stderr for invalid input. Both inputs are
strict JSON documents of at most 4 MiB; RUNS is an array of at most 54 records,
not a partial JSONL stream. Malformed or truncated JSON is rejected, not silently
repaired. Nothing in either document is executed or fetched, and the command makes
no network request. `just check` exercises it on synthetic fixtures.

The summary has `status` (`pending`, `incomplete` or `complete`), then `reason`
for a pending manifest or `manifestSha256` otherwise, then one group per cache
policy and arm with its run counts, failures and metric totals, and one decision
per candidate arm. Its bytes match agenthome's summarizer for the same inputs:
[summary.json](../eval/summary.json) is its output for the checked-in
[manifest](../eval/manifest.json) and empty [runs](../eval/runs.json), and the
tests compare the two byte for byte.

## Admission

The concrete threat is an evaluation that spends more than ordinary work could:
cells added or retried until an arm looks good, or runs that bypass the pool's
pace and slots. A measurement therefore needs no new spending authority,
reservation or broker change. It may start once the manifest is frozen, under
existing bounds that the cell's configuration must name; reviewing that
configuration before the first cell is what holds it to them:

- **agentic-job's per-run limits.** Each cell is one run whose
  [limits](https://github.com/cgwalters-forge/agentic-job/blob/3d0c9f79ef74791bef608718b2cc2dbe7101cc46/docs/harness.md#limits)
  set a timeout of at most the manifest's `limits.wallSeconds` and a
  `max-requests` cap of at most `limits.turns`, plus `max-tasks` where the arm
  starts subagents, sized from the preflight, and never `uncapped`. agentic-job
  refuses a run without a timeout, but accepts a dollar `budget` in place of
  `max-requests`, so the request cap is a rule of this protocol. `max-requests`
  acts before a request; the `budget` is checked after each reply and can
  overshoot, so the manifest's `usd` and `tokens` limits are sizing ceilings
  that a cell's recorded metrics are judged against, not hard caps. The
  summarizer does not check them. `tokens` counts input plus output tokens,
  as the [2026-11-17 rescope](https://github.com/cgwalters-forge/agent-board/issues/1038#issuecomment-6963338151)
  decided; before it, the ceiling was read as the total including cache reads.
  Cache reads are excluded from it but never dropped: the record keeps them
  as `cacheReadTokens`. Nothing enforces a cache-read limit: the token screen
  no longer covers them, and the `usd` screen only weighs them after the fact,
  at a price that may differ from the historical list price and with a budget
  that can overshoot. A cache-heavy cell can therefore exceed 200000 total
  tokens and still fit. What actually bounds a cell is its timeout and
  request cap, so the cell configuration must set both and name the pool's
  cached-input price used to judge `usd`.
- **The board's pool pace and slot accounting.** Each cell takes a
  [lease](pass.md#leases) with `agent-board admit` for the evaluation's tracking
  issue before any inference and frees it with `agent-board release`, so it
  counts against `wip`, its pool's pace and the weekly budget like any other run.
  The caller's dispatch does not take leases yet, so the harness running the
  cells adds these two steps. `admit` refuses rather than waits; start a refused
  cell later, in the predeclared order. It has not started, so it occupies no
  cell.
- **A total cell count declared before the first cell.** Freezing the manifest
  fixes tasks × 3 arms × 2 cache policies × 3 repetitions, 36 or 54 cells, and
  every run carries the manifest's digest. No cell is added, retried or replaced
  after it starts; a timeout, exhausted limit or harness failure occupies its
  cell.

Anything that would exceed those bounds stays with the operator: a lease past
`wip` or pacing needs `admit --override` with the operator's token, and a cell
without a timeout and request cap, or a measurement beyond the frozen cell count,
needs the operator's explicit go-ahead.

The [preflight report](../eval/report.md#archived-2026-11-03-preflight) retains
the 2026-11-03 state, when the protocol instead waited on verified pool capacity
and an evaluation reservation. Its current preparation section supersedes that
gate and records historical sizing. Draft
[agenthome#87](https://github.com/cgwalters-forge/agenthome/pull/87)'s
allocation-evidence checker served that gate and is not carried here.

## Freezing the sample

`manifest.json` is deliberately **pending**, not an executable sample. It retains
two historical tasks and check bundles whose sizing fits every ceiling under the
input-plus-output token definition above, though not under the earlier total
including cache reads; see [the report](../eval/report.md#historical-sizing). Resolved configuration and measured results
remain pending; synthetic fixtures are not research evidence.
`runs.json` is empty, and `summary.json` is its offline summary, not a measured
comparison.

Inspect two or three real historical fix-round or small-implement tasks and their
transcripts. Record links, observed size/runtimes and why each task fits the
limits; do not select by a favorable arm outcome. Verify the starting commit is
before the fix, reproduces the problem, and can run independent checks. Freeze
the sample before any comparison: retain the original request text, immutable
full starting commit, historical issue/PR links, a pinned independent check
bundle and sizing evidence per task. Validate that checks fail on the start and
pass on the historical fix; keep expected solutions/checks outside agent context.
All arms receive identical task text, tools, environment, permissions and
check-independent context. Do not cherry-pick the historical fix.

A clean worktree and a check bundle kept at an outside path are not enough to
keep the oracle out of agent context: the historical fix, later commits and,
once merged, the check patches under `eval/tasks/` are reachable through Git
history and forge reads. Before freezing, the configuration must give each arm
a repository holding only the start commit's history (no remote, no later refs
or tags, and no `eval/tasks/` bundles) and no forge read access
to the task's history links or later pull requests, and the preflight must
record a check that the fix commit is absent from the arm's object store and
that those reads are refused. Without it, a passing check may measure retrieval
of the solution rather than the task.

Pending manifests may retain either no tasks and null preflight, or two or three
fully pinned candidates with preflight. They require a nonempty pending reason,
null configuration and empty runs in either case. Preparing candidates does not
admit execution.

`preflight` holds `authorization` and `evidence` links: the operator's go-ahead
naming the permitted repositories, harness, models and artifact destination, and
the sizing record. Each `tasks` entry has `id`, `repository`, `history`,
`startCommit` (40 hex), `taskText` (literal common text), `checks: {artifact,
sha256}` (64 hex), and `sizingEvidence`. Change status to `frozen` only with
evidence and the configuration below. Manifest, tasks, checks and configuration
must be immutable artifacts for the comparison; record their content digests in
the research report. Every run carries `manifestSha256`, computed by
`manifest_digest` in `crates/agent-board/src/eval.rs` (SHA-256 of the
`JSON.stringify` spelling of the manifest with recursively sorted object keys,
preserving array order, so digests from agenthome's JavaScript and Rust
summarizers stay valid). The summary includes this digest. It binds the entire
sample, limits and policies, not just the current run's task. A changed
task/check/config requires a new full comparison, never pooling the two.

## Arm bundles and authorized execution

[arms.json](../eval/arms.json) declares the three opt-in arm bundles, carried
from draft agenthome#87, as references into agenthome's checked-in files rather
than copies of role prompts or concrete model IDs. Every arm starts from
agenthome's `opencode/opencode.json` and `opencode/opencode-runner.json` and
resolves models through `prompt-tiers.json`. `single-strong` and `single-cheap`
keep the ordinary direct-worker prompts and permissions and set every configured
and built-in agent, the embedded reviewer included, to one resolved tier
throughout. `tiered` adds the role agents of `roles/opencode/eval-tiered.json`
(Claude Code's are in `roles/claude/agents/`) with the models that file sets
per role, so agenthome stays the one source of the role mapping. `arms.json`
names no revision: step 1 below pins agenthome's. Ordinary hosted settings are
unchanged; each bundle is an opt-in research configuration only. Provider
addresses and keys stay the checked-in placeholders for the harness to replace
from its environment.

A frozen `configuration` contains `configRevision`, `harnessRevision` (full
commits), `roleInterface` (pinned link to the role configuration), `environment`
(immutable image/toolchain identity), and `arms`. Each of `single-strong`,
`single-cheap`, `tiered` has `{artifact, sha256, resolvedModels}`.
`artifact` links to the complete assembled bundle; `resolvedModels`
is a canonical serialized role-to-resolved-model/version map, not aliases;
`configuration.costBasis` is one common `{artifact, sha256}` pin covering the
price table/currency/date and token/cache charging semantics for all arms, or
exactly `{unavailable: "reason"}`. Do not mix billed baseline costs with estimated
candidate costs; normalize all arms to the common basis or declare costs
unavailable. Each run repeats that same cost-basis object. Keep the base config
revision common; the per-arm bundle digest captures the intentional arm
difference.

For an authorized research run:

1. Pin agenthome's configuration revision and the harness, assemble the three
   bundles from the files `arms.json` names at that revision, and archive them and
   the resolved model maps. Configured IDs are not proof of provider model versions.
2. Verify the actual runner's installation contract. agentic-job installs a
   source `opencode-runner.json` profile but not source `agents/*.md` bodies, so
   custom role agents are **not** necessarily installed. Prove the selected
   runner loads each role and model setting, the embedded reviewer and internal
   agents included, and archive the final merged files after all
   harness-generated agents and overrides. If that cannot be shown, stop and
   record the dependency as pending. Do not assume acp-drive can spawn arbitrary
   tool/model children. A manual staged run must be labeled as such and not
   pooled with automatic sessions. Archive exact inter-stage context handoffs.
3. Start a clean worktree at the frozen commit for each cell, isolated from
   later history and forge reads as [freezing](#freezing-the-sample) requires,
   within the [admission](#admission) bounds above, in the predeclared order. No retries or
   automatic top-ups.
4. Run independent checks outside agent control; archive commands, exit codes,
   logs, patch and transcripts. Normalize exposed telemetry to the records below.
   Missing provider fields remain unavailable, never inferred from context size.
5. Summarize offline with `agent-board eval summarize MANIFEST.json RUNS.json`.

## Fair comparison and predeclared rule

Run three repetitions of every task/arm/cache cell: 36 runs for two tasks or
54 for three. Predeclare and archive a balanced, rotated arm order per task and
repetition before starting; schedule strata close together and record provider
version changes. No best-of selection, silent retries or excluding failures.
Keep temperature, seeds when supported, context policy and permissions fixed.

Cold means no intentional priming and a fresh session/cache namespace, with
provider cache isolation verified. Warm means one identical, authorized priming
procedure per arm/cell using only the common initial context, not solutions or
earlier transcripts. Archive the procedure and include priming cost and time in
that cell's metrics and limits. If isolation/priming cannot be established, stop
that stratum rather than labeling an unknown cache state cold/warm. Three
repetitions are independent sessions, not conversation continuations.

`quality-cost-time-v1` is frozen before comparison: each candidate is eligible
only if **every** candidate and strong-baseline run passes the independent checks,
its total USD is at most 80% of baseline and its total wall time is at most 110%
of baseline, separately in both cold and warm strata. Inclusive comparisons
tolerate only binary roundoff (32 machine epsilons relative to the compared totals).
All runs count, including failed runs' spending and elapsed time. An incomplete
matrix or unavailable cost/time yields inconclusive; a quality failure rejects.
Zero baseline cost/time cannot provide a ratio. Eligibility is permission to
propose a subsequent authorized adoption experiment, not evidence of statistical
generality or an automatic default switch. This tiny historical sample cannot
establish that.

## Run records and unavailable telemetry

Each version-1 run records `id`, `runUrl`, `artifactUrl`, `manifestSha256`, `taskId`, `arm`,
`cachePolicy`, `repetition` (1–3), `startCommit`, literal `taskText`,
`checksSha256`, `configRevision`, `harnessRevision`, `environment`, `renderSha256`,
`resolvedModels` and `costBasis`. They must match the frozen manifest. Links must
remain accessible to the authorized reviewer; never publish credentials or raw
secrets in fixtures or logs. Different revisions, duplicate IDs/cells and runs
outside the matrix are errors. Per-run links retain provenance when aggregating.

`metrics` requires `inputTokens`, `outputTokens`, `cacheReadTokens`,
`cacheWriteTokens`, `costUsd`, `wallSeconds`. Each is `{value: number}` or
`{value: null, unavailable: "reason"}`. Numbers are finite and nonnegative;
unsupported is not zero. State in the pinned cost basis whether input includes
cache tokens and whether cost is billed or estimated; never sum incompatible
bases. Include all child/stage usage once, not both parent totals and children.
If complete totals cannot be recovered from truncated telemetry, set affected
metrics unavailable even if a partial count exists. Unaffected externally timed
wall time or billing totals may still be known. `telemetry` has `status`
(`complete`, `missing`, `truncated`) and a `reason` for incomplete capture.

`stageSeconds` maps exposed stage names to the same metric objects or is exactly
`{unavailable: "reason"}`; no fabricated zero-duration stages. `outcome` is
`passed`, `failed`, `timeout` or `harness-error`; `failures` is an array of reasons
(required for non-passes), and `testEvidence` links to independent check results
or the log explaining why checks could not run. A process exit alone is not
correctness. The summary reports failures and availability per metric; a single
missing value makes that aggregate unavailable, not a partial total (and a total
too large for a double is null, as JavaScript writes it). Keep raw run records
alongside summary output for individual links, timing and cost audits.
