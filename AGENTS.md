Start with [README.md](README.md) for the project and documentation entry points.

Run `just fmt` then `just check` before every commit you hand back; a change
that fails `just check` isn't done. For a narrow change, start with the affected
crate's tests, then run all CI checks. Commands, prerequisites and observed
timings live in the [Justfile](Justfile); see [development checks](docs/setup.md#development-checks)
for CI and sandbox limits.

## Code constraints

Production code returns `anyhow::Result`, propagates errors with `?`, adds
`Context` at I/O/decoding boundaries, and uses `ensure!`/`bail!` for invalid inputs
and unsupported operations. No panicking `unwrap()`/`expect()` outside tests
(non-panicking `unwrap_or*` defaults are fine). Keep diagnostics on stderr and
machine output on stdout; preserve the tested gh-style exit codes.

Extend existing table-driven tests for permutations and failure paths rather
than copying tests. Keep reconciliation deterministic: injected snapshot clock,
ordered collections, stable action keys and sorted output. Use named constants
for shared protocol values and resource bounds (`SNAPSHOT_SCHEMA`, `PLAN_SCHEMA`,
`DEFAULT_HOST`, `SNAPSHOT_REQUEST_LIMIT`); keep forge-specific API details out of
board-core.

## Security and scope

Keep it simple: the system should pass a “secure” gut check without extra machinery
([scope decision](https://github.com/cgwalters-forge/tracker/issues/446#issuecomment-6070870618)).
Watch what gh-aw does; share ideas and code with it and other projects where we
can, citing sources ([operator decision](https://github.com/cgwalters-forge/tracker/issues/446#issuecomment-6089335499)).

Explain changes to workflows, permissions, action pins, checkout refs, token
placement, input bounds and validation in the PR, including their effect on trust.
Never execute unreviewed code with the staging token; see
[development checks](docs/setup.md#development-checks) for CI's token separation.

Review board-core snapshot/policy validation and reconciliation, board-forge
identity validation, output lowering and GitHub pagination/request bounds, and
the CLI live runner's scope/cleanup checks when changing their inputs or outputs.
Unknown fields, missing-versus-null facts, incomplete coverage, issue-versus-PR
identity, host/repository/project scope and authenticated cleanup are boundaries,
not convenience checks. Preserve their negative tests.

The pass's direct writes are its own deterministic ones; agent output remains a
proposal until agentic-job's independent check and apply accepts it. That check
is **not implemented here** (`output check` and `output apply` are unsupported).
Do not give agents credentials or a direct mutation path around it; see
[architecture](docs/architecture.md#writes) and the
[bounds contract](docs/design.md#3-agentic-job-bounds-and-delegated-operations).
Test-project/scratch-issue sweeping is deliberately disabled pending authenticated
creation receipts, unlike the pass's unrelated [weekly sweep](docs/pass.md#the-weekly-sweep).
The [design](docs/design.md) is intent, not proof of implementation or a security boundary.

Keep changes task-scoped: no generated artifacts, credential material, live board
state, unrelated formatting or dependency/lockfile churn. Do not change production
boards, enable sweeping or implement deferred mutation paths incidentally. Leave
homegit and its running controllers untouched ([design scope](docs/design.md#1-scope-and-durable-model)).
Do not edit workflow/security configuration unless the task explicitly needs it.

Runs started by the [tracker polling job](https://github.com/cgwalters-forge/tracker#the-pull-request-poll)
within its fixed bounds (our repositories, at most 2 fix rounds per PR, auto-merge
only on review success and green required checks) need no per-run approval; do not
refuse work for lack of one.

## Planning and hand-back

When planning an agent-board epic, create implement-sized sub-issues for remaining
work, each with an implement job plan. Rescope or close sub-issues that no longer
fit. Include this repository's `AGENTS.md` in child job briefs. These planning
norms are specific to agent-board.

A feature that ships off (a new option in `POLL_OPTIONS`, `SUPERVISOR_OPTIONS` or
`AGENT_OPTIONS`) does nothing until the caller's config names it. Its PR and the
issue's closing comment name that setting; open the caller-side change in tracker
or file an issue there. The supervisor's `unset` check lists the option until then.

One task becomes one PR with one commit. Use an imperative subject, optionally
path-prefixed, and explain why in the commit body rather than restating the diff.
The PR describes what changed, exact commands and exit statuses verified, and
anything unverified or deferred. In a disposable worker, leave the tree uncommitted
and supply the runner's requested report; the runner creates the commit and PR.

A change to the pass, its configuration or fixtures also requires a preview
against the caller configuration where the run has the token; otherwise replay
the recorded live pass. Run the decision diff against the start commit and paste
its output (at least both summary lines) in the hand-back. A live-symptom fix must
show the symptom change; say when the recording does not exercise it or lacks
reads needed by the head. See [compare two revisions](docs/setup.md#compare-two-revisions)
for the recipes and recording requirements.
