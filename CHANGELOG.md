# Changelog

User-visible changes to the `agent-board` and `gh-agent-board` binaries,
their configuration files and the records they write on the forge, newest
first. agent-board has no release yet; deployments pin a reviewed commit
(docs/setup.md#pinned-callers). The [v1 readiness checklist](docs/release.md)
lists what v1.0 waits on and what a caller may rely on.

## Unreleased

The first release will be v1.0.0, once every gate in docs/release.md is met.
Since the workspace started at 0.1.0 it has gained:

- A live Forgejo v17.0.1 proof in CI (docs/forges.md#live-ci). It found that
  Forgejo starts were all cancelled as wrong runs, since the server reports a
  dispatched run's `event` as empty; the adapter now accepts that, and
  `trigger_event` still has to name the dispatch.
  The Forgejo token also needs `read:organization` for label writes.

- Forgejo pass `--record` and `--replay`: a preview's forge and broker reads
  saved in order, with 404s, totals and failures, to a fixture pinned to its
  instance, and replayed offline with no token or client
  (docs/forges.md#recording-and-replay). Seeded recordings stay GitHub-only.

- Forge-aware pass name admission: dotted Forgejo logins and repository owners
  now reach identity preview, without widening GitHub rules or allowing names
  to alter API paths. Recorder-id verification and preview-only behavior remain.
- Forgejo Status label reads behind `board.statusLabels` (off by default),
  and edit-provenance checks for recorder Run/Result markers. The library
  has bounded, previewable board label/state/comment primitives and
  append-only plan claims and receipts with a recovery read; no Forgejo
  CLI write path, plan apply or dispatch is enabled yet (docs/forges.md).
- `agent-board pass`, one scheduled pass from one configuration file
  (docs/pass.md): pull request follow-up with bounded fix, rebase and
  continuation rounds, epic planning and apply, scheduled starts by
  Priority, WIP and pool pacing, board Status, reviews, reruns, start-failure
  and stall alarms, the weekly sweep, the maintain digest, leases and the
  hold snapshot for the review app. It previews unless `--apply`, within
  `--max-dispatch` and `--max-writes`, and `--replay` previews a recorded
  pass offline.
- The pull request poll, `agent-board poll` (docs/poll.md), and the
  supervisor's health pass, trigger and executor, `agent-board supervise`
  (docs/supervisor-requests.md).
- Job and epic plans, `agent-board dispatch`, `epic plan` and `epic apply`
  (docs/job-plans.md, docs/epic-plans.md), and operator actions and asks
  (docs/operator.md).
- Read-only reports: `budget`, `cost` (docs/cost.md), `prune`
  (docs/prune.md), `changes` and `status`.
- `release-train`, `broker probe`, `apps create` and `review send`
  (docs/release-train.md, docs/broker-probe.md, docs/apps.md,
  docs/review-drafts.md).
- Offline board snapshots, item queries with gh-style `--json` and `--jq`,
  and preview-only reconciliation (docs/cli.md).
