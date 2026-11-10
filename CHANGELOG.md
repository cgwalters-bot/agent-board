# Changelog

User-visible changes to the `agent-board` and `gh-agent-board` binaries,
their configuration files and the records they write on the forge, newest
first. agent-board has no release yet; deployments pin a reviewed commit
(docs/setup.md#pinned-callers). The [v1 readiness checklist](docs/release.md)
lists what v1.0 waits on and what a caller may rely on.

## Unreleased

The first release will be v1.0.0, once every gate in docs/release.md is met.
Since the workspace started at 0.1.0 it has gained:

- Session batches applied, the third slice of agent-board#830
  (agent-board#845): with `"apply": true` in the pass configuration's
  `sessions` section (off by default), each pass after a day's session
  started applies the batch answering it once a day, within the bounds
  its brief named and `--max-writes`, posting the receipt on the anchor
  as the claim before any write, resolving each issue named as an issue
  (never a pull request) before the first write, and applying no second
  batch on a day with a receipt; a refused batch, a cut-short apply or an
  unreadable receipt holds the anchor for the operator until the next
  session. Closes, rescopes, Priorities, epic links and posted plans are
  written as the bot; questions are listed on the receipt for the
  operator (docs/sessions.md#applying-a-batch).

- Daily sessions, the second slice of agent-board#830: with the pass
  configuration's `sessions` section (`hourUtc` and the anchor issue of
  each work repository that gets one, off by default), stage 4 starts one
  `session` run a day on each anchor, before its per-item starts and
  within the same WIP, pacing, budget and `--max-dispatch`, its brief
  naming the caller's bounds and the offline batch check; the dispatch
  record on the anchor is the day's receipt, the anchor is no candidate
  of its own, and a session run in flight now counts as a dispatch run
  (docs/sessions.md#daily-sessions).

- A `session` job plan kind, the first slice of triage per repository in
  one long run instead of many one-comment item triages (agent-board#830,
  the per-repository maintainer of tracker#1356): a session answers with
  one `agent-board-session` batch of at most 20 typed actions, each kind
  capped (5 closes, 5 rescopes, 10 Priorities, 5 epic links, 5 posted
  plans, 3 one-line questions to the operator) and checked against the
  caller's repositories, the board's Priority options and the
  `operatorAsk` domains; `agent-board check session-batch` validates one
  offline. The pass starts accepted session plans only with `sessionPlans`
  (off by default), and then only the operator's own, never the planner's;
  no batch is applied yet (docs/sessions.md).

- Harness work, issues in the work repositories of `dispatchRepo`'s owner
  outside `pr.noMerge` and the midstream forks, no longer waits on the
  operator but for their own decisions (agent-board#778): an asking triage
  verdict there is triaged again with a brief to plan it, unless it names
  `"operator"` (credentials, budget, ruleset, upstream or bounds), is a
  close to confirm, or the issue carries an operator label. With
  `assignHolds`, the bot's existing assignments and `needs-human` labels on
  such work are released, at most 5 a pass, each with one
  `<!-- agent-board-release -->` comment (docs/pass.md#scheduling).

- The hold snapshot on GitHub is no longer cut to one 65,536-character
  comment, which had left out every needs-you detail card and the pass view
  (agent-board#774): one longer than a comment is gzipped, in base64, across
  at most 4 of the bot's comments on the holds issue, the first with its
  usual marker and a header that counts them, the others with
  `<!-- agent-board-holds part v1 -->`. Readers must assemble it; the review
  app does from its matching release (docs/pass.md).

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
