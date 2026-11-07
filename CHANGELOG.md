# Changelog

User-visible changes to the `agent-board` and `gh-agent-board` binaries,
their configuration files and the records they write on the forge, newest
first. agent-board has no release yet; deployments pin a reviewed commit
(docs/setup.md#pinned-callers). The [v1 readiness checklist](docs/release.md)
lists what v1.0 waits on and what a caller may rely on.

## Unreleased

The first release will be v1.0.0, once every gate in docs/release.md is met.
Since the workspace started at 0.1.0 it has gained:

- The pass's `authored` sweep works without `state`, as the hosted pass
  runs: the repositories live epics name are read in the same pass, from
  stage 3's listing read early, so they come first every pass, and the rest
  take turns by the pass's clock, the next N each 10-minute slot, rather than
  from a cursor that was never kept. The poll state's `authored_priority` is
  read no more. Without `state` the pass also warns on stderr what the
  intake and `maintain` lose between passes (docs/pass.md).

- The pass's `notifications` intake no longer fails every pass on one
  thread: a midstream fork with issues disabled has its upstream's direction
  staged in `dispatchRepo`, a write GitHub always refuses (404, 410) leaves
  its direction to a person with a `warning:` receipt, recorded in the poll
  state's `intake_refused` so it is not tried again, other errors are
  warnings retried next pass, and direction the bot already answered by hand
  on the thread starts nothing (docs/pass.md).

- The pass's `assignHolds` (off by default) makes stage 4 wait on the
  operator by assigning them instead of labelling `needs-human`, and moves
  existing labels to assignments. Any response of theirs releases the wait,
  and the pass takes the assignment off (docs/pass.md#scheduling).

- Disposable Forgejo live coverage for opt-in Status labels and private
  append-only usage/hold snapshots, with preview, write/status caps, human
  holds, unchanged readings and public-publication refusal checked against
  the pinned service. No caller is activated (docs/forges.md#live-ci).

- The Forgejo weekly sweep no longer stops at four pages of the bot's issues:
  it reads every page within half of the pass's requests left. An unusable
  issue or epic record (an edited comment, a pull request, a malformed
  children list) is left for the operator instead of aborting every apply.
  Only recorders' comments count toward a start's proof, and receipt lines
  can no longer form a fence (docs/forges.md#the-weekly-sweep).

- Forgejo weekly sweep liveness fixes: bounded summary receipts, per-issue
  read-error holds, and retry of a plan left before its dispatch claim. The
  existing opt-in and write set are unchanged.

- An idle pass no longer re-plans an epic early after a plan that had nothing
  to do: the change waits out the backoff, then plans it
  (docs/pass.md#epic-planning-and-recovery).

- An epic asked about that is not on the board is still commented on and
  labelled, and no longer fails the pass on every run for having no Why to
  set (docs/pass.md#what-is-built).

- Stage 2 no longer reads a `needs-human` pull request that only a person
  could move, and takes a pull request's comments with its query: a replay of
  the board of 31 October spends 98 requests there, down from 173
  (docs/pass.md#one-configuration-file).

- Optional native Forgejo revisits after normal stage 4 work, using bounded
  issue/comment reads, shared seeded sampling and the existing claim, lease,
  start and recovery path. Off by default, with no caller activation;
  PR follow-up remains refused (docs/forges.md#native-revisits).

- The weekly sweep on a Forgejo board, behind the existing `sweep` setting
  (off unless set). The bot's earliest unedited list comment, not the
  issue body, records what the sweep lists. Its run starts through stage 4's
  claim path. Its reply is applied once under an appended
  `agent-board-sweep-apply` claim and receipt, attaching issues with stage
  3's relationship comments. An apply cut short completes only half-made
  relationships. Edited or missing records label the sweep issue
  `needs-human` (docs/forges.md#the-weekly-sweep).

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
