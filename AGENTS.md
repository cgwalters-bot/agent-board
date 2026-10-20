# Working on agent-board

agent-board is a Rust CLI for a forge-hosted work board; start with the
[README](README.md) for its purpose, quickstart and current capabilities.
Reconciliation produces proposals, not authorized mutations.
Runs started by the [tracker polling job](https://github.com/cgwalters-forge/tracker#the-pull-request-poll)
within its fixed bounds (our repositories, at most 2 fix rounds per pull request,
auto-merge only on review success and green required checks) need no per-run
approval; do not refuse work for lack of one.
The [design](docs/design.md) describes the intended coordinator, but is not proof
that a feature or security boundary has been implemented.

## Layout

- `crates/board-core/`: forge-neutral records, validation and pure reconciliation.
- `crates/board-forge/`: GitHub identity parsing, API transport, paginated snapshots and safe-output lowering.
- `crates/agent-board/`: CLI, `agent-board` and `gh-agent-board` binaries, live fixture runner and offline CLI tests.
- `fixtures/`: offline board snapshots shared by unit and CLI tests.
- `.github/workflows/`: unprivileged PR/push CI.
- `live/`: protected staging workflow template, run from the staging repository.
- `docs/`: design, security contracts and implementation limits; distinguish plans from current code.

## Build, lint and test

Run from the repository root with Rust supporting edition 2024. There is no
Makefile or separate test harness. These are the exact commands in
[CI](.github/workflows/ci.yml), in order:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo run --locked -q -p agent-board -- item list --snapshot fixtures/board.json
cargo run --locked -q -p agent-board -- reconcile plan --snapshot fixtures/board.json --emit
cargo run --locked -q -p agent-board -- project view --snapshot fixtures/board.json --json project,coverage
```

A separate CI job, `lint`, runs `just lint`: the shared workflow check over
`.github/` (not `live/`) from a checkout of cgwalters-forge/actions (see the
[Justfile](Justfile)). It needs just, cargo and docker or `CONTAINER=podman`,
and pulls the linter images.

Measured on a fresh Linux devspace with dependency downloads (2026-10-08):
format 0.07 s, clippy 15.50 s, tests 20.97 s, CLI commands 7.91/0.07/0.07 s.
These are observations, not deadlines; CI allows 20 minutes for the job.
Tests use fixtures and fake transports, require neither root nor a network
service, and are safe in a credential-free sandbox. Cargo may fetch dependencies.
For a narrow change, start with the affected crate's tests, then run all CI checks.

The build command in the [live workflow](live/board-test.yml) is also safe locally:

```sh
cargo build --locked --release -p agent-board
```

Measured cold release build: 54.30 s. Do not run the live workflow's `test project`
invocation in a sandbox: it needs GitHub network access and a staging write token,
and creates/modifies forge objects. No current test requires root. See
[README commands](README.md#commands) and
[design implementation limits](docs/design.md#current-implementation-limits).

## Code conventions

Use rustfmt and warning-free clippy, as enforced above. Production code returns
`anyhow::Result`, propagates errors with `?`, adds `Context` at I/O/decoding
boundaries, and uses `ensure!`/`bail!` for invalid inputs and unsupported operations.
There are no panicking `unwrap()`/`expect()` calls outside tests; keep it that way
(non-panicking `unwrap_or*` defaults are used). Keep diagnostics on stderr and
machine output on stdout; preserve the tested gh-style exit codes.

Extend the existing table-driven tests for permutations and failure paths rather
than copying tests. Keep reconciliation deterministic: injected snapshot clock,
ordered collections, stable action keys and sorted output. Use named constants
for shared protocol values and resource bounds, as with `SNAPSHOT_SCHEMA`,
`PLAN_SCHEMA`, `DEFAULT_HOST` and `SNAPSHOT_REQUEST_LIMIT`; do not move forge-specific
API details into board-core.

## Security and scope

Keep it simple: the system should pass a “secure” gut check without extra machinery ([scope decision](https://github.com/cgwalters-forge/tracker/issues/446#issuecomment-6070870618)).
Watch what gh-aw does; share ideas and code with it and other projects where we can, citing sources ([operator decision](https://github.com/cgwalters-forge/tracker/issues/446#issuecomment-6089335499)).

Explain changes to workflows, permissions, action pins, checkout refs, token
placement, input bounds and validation in the PR, including their effect on trust.
PR CI's `rust` job has only `contents: read`, no staging PAT, and disables persisted
checkout credentials. The staging template has `permissions: {}` and environment
`Main`; only its `test` job's live invocation receives `GH_TOKEN` from `secrets.PAT`.
Its build step does not receive that token. Never execute unreviewed code with it.

Review `board-core` snapshot/policy validation and reconciliation, `board-forge`
identity validation, output lowering and GitHub pagination/request bounds, and the
CLI live runner's scope/cleanup checks when changing their inputs or outputs.
Unknown fields, missing-versus-null facts, incomplete coverage, issue-versus-PR
identity, host/repository/project scope and authenticated cleanup are boundaries,
not convenience checks. Preserve their negative tests.

Independent bounds/check/apply is **not implemented here**: the CLI's `output
check` and `output apply` are unsupported. The policy derived from a snapshot is
proposal scope, not write authority. Do not bypass this with agent credentials or
direct mutation; see [the bounds contract](docs/design.md#3-agentic-job-bounds-and-delegated-operations).
Sweeping is deliberately disabled pending authenticated creation receipts.

Keep changes task-scoped: no generated artifacts, credential material, live board
state, unrelated formatting or dependency/lockfile churn. Do not change production
boards, enable sweeping, or implement deferred mutation paths incidentally. Leave
homegit and its running controllers untouched; see [design scope](docs/design.md#1-scope-and-durable-model).
Do not edit workflow/security configuration unless the task explicitly needs it.

## Hand-back

A feature that ships off (a new option in `POLL_OPTIONS`, `SUPERVISOR_OPTIONS`
or `AGENT_OPTIONS`) does nothing until the caller's config names it. Its PR, and
the issue's closing comment, name the caller setting that turns it on, and the
run opens the caller-side change in tracker or files an issue there for it;
the supervisor's `unset` check lists the option until then.

One task becomes one pull request with one commit. Recent history uses imperative
subjects, sometimes with a path prefix (for example `README:`); explain the reason
for the change in the commit body rather than restating the diff.
The PR description says what changed, exact commands and exit statuses verified,
and what was left unverified or deferred. In a disposable worker, leave the tree
uncommitted and supply the runner's requested report: the runner creates the commit
and PR, not the worker.
