# One pass

This is the proposed contract for `agent-board pass`, not implemented behaviour.
Approve this contract and its drop list before implementing the command. A pass
finishes once; it is not a daemon, a second planner, or a JavaScript compatibility
layer. Existing [PR follow-up](poll.md),
[job-plan](job-plans.md) and [epic-plan](epic-plans.md) protocols remain the
authority for their operations. The differences below are intentional.

## Order and effects

`agent-board pass --config FILE` previews by default. Only `--apply` permits
writes. The caller serializes apply passes for the board, uses trusted
default-branch configuration and a pinned build, and supplies credentials through
the environment, never the file. Issue text, PR bodies, run names and comments
other than authenticated operator direction are data, not instructions.

A pass captures one clock and performs these stages in order:

1. Follow up PRs using today's `poll` decisions: read followed repositories,
   PR heads, reviews, checks, comments and runs; dispatch bounded fixes/reviews,
   merge only an approved head with green required checks, or comment/label for
   a person. Retain its claim and head-recheck rules. Count every dispatch made
   here against capacity before starting anything later. Pacing here reads the
   previous trusted usage snapshot; missing/stale readings hold new PR runs,
   including on a cold start. The refresh in stage 2 prepares later stages and
   the next pass, not retroactive permission for stage 1.
2. Refresh usage from the configured broker, retaining the existing normalized
   paced-window calculation (target, burst, short-window exhaustion and original
   observation time). Read the previous trusted snapshot and publish the new
   `bot-usage/v1` snapshot to the configured private issue. Preview computes it
   without publishing. A failed refresh is visible; it cannot turn missing or
   stale readings into spare capacity.
3. Read the complete board, Priority options, live epics, their children,
   authenticated operator comments, accepted plans, apply receipts and current
   runs. Recover completed epic applies, apply an accepted pending epic plan,
   or start at most one due epic-planning run. Epic apply may create/link issues,
   set Priority, rescope or close children and post plans/receipts within the
   existing epic bounds; planning itself only dispatches a triage run. Refresh
   affected board/issue facts after an apply before scheduling.
4. Choose and dispatch work by the rule below. Dispatch writes the existing
   plan claim and run link, and the existing board Run update where supported;
   it does not invent Status transitions or close issues from guessed outcomes.

The result names actions, holds, incomplete reads and failures, with a nonzero
exit for an operational failure. Independent PRs may still be followed when an
epic cannot be read. Do not schedule from an incomplete candidate set or missing
hold facts. Re-read the chosen plan, holds and capacity before each dispatch;
reserve its slot before sending, including when the response is ambiguous.
There is no transaction across stages: preserve receipts for writes already
made, stop the affected operation, and recover by observation on the next pass.
Deleting a local cache must never authorize replay.

To stop an unbounded board scan or runaway dispatch loop, use one fixed pass
budget: at most 500 forge requests and 15 minutes, including existing subcommand
work; existing tighter read/action bounds still apply. Report exhaustion and
defer unresolved decisions, never interpret truncated data as empty. There is
no second fill-page budget. Preview performs no forge writes or dispatches and
does not change the apply cache.

## Scheduling

There is one candidate set: open board issues in explicitly configured work
repositories, excluding epic/question/control issues. An epic child qualifies
only through a live epic whose latest `epic` label event is the operator's.
An unparented issue qualifies only when opened by the operator or bot; public
visitors must not be able to spend runs simply by filing issues. Deduplicate by
repository and number. No separate under-pace candidate source exists.

Order by Priority (P0, P1, and so on; unset/unrecognized last), then oldest
creation time, then repository/number for stable ties. For epic children compare
epic Priority first, then child Priority, then age; an unparented issue uses its
own Priority as the first rank. A child in several live epics uses its highest
ranked parent. Priority is the only readiness ordering; there is no Ready step.

`wip` is the total cap on noncompleted caller-workflow runs, including queued,
waiting, planning, triage, implement, review and fix runs. Include starts earlier
in this pass even if the API has not listed them. A pre-existing excess is not
cancelled; it leaves no new slots. PR repair retains its own existing bounds;
if it consumes/exceeds WIP, later stages start nothing. No planning exemption.

Respect `needs-human`, `needs-design`, board Blocked, open dependencies, an open
PR for the issue or plan target, a run in flight, and the existing bot-PR/path
overlap hold, including draft rescue PRs. Keep explicit stacking semantics;
never guess a stack from overlap. An unreadable hold is not permission to start.
Check both the board issue and a plan's target; neither may escape configured
repositories. To avoid duplicate/conflicting work, pass always enables the
existing bot overlap check and conservatively waits when it cannot complete it;
this is stricter than standalone dispatch's best-effort heuristic.

With no accepted plan, start one triage run if enabled and a slot/pool is
available. It proposes an implement plan, or explains a needs-design/human hold;
it does not close the issue on a `done` verdict. Record the question using the
existing dispatch claim protocol, not another scheduler marker. A completed run
without a usable plan remains visible for a person; do not automatically retry.
With an accepted implement plan, dispatch precisely that plan once. An invalid
newest plan does not mean “no plan”; report it, never fall back. Recorded plans
do not run again; an operator/planner edit or replacement is needed. Research or
triage plans explicitly posted on issues remain manual dispatches.

Choose among the configured automatic pools by greatest room below pace:
`target_percent * elapsed_fraction - used_percent`; ties use config order.
Exclude held, exhausted, reset, missing or stale readings (snapshot older than
45 minutes or paced observation older than 30 hours). If none qualifies, wait;
do not fall back to a held/unknown pool. Negative room alone does not exclude a
pool if its normalized burst-aware hold is false. An explicit plan pool is
respected, not silently replaced, and must pass the same checks. Pacing applies
to planning and triage too. Capacity is filled by this same rule whether under
or over pace; there is no fill mode, retry pool or shortened planning interval.

## Epic planning and recovery

A live, unheld epic is due initially, after changed intent/operator direction
or membership/state, body or Priority changes in its children, or after 24 hours
since its last resolved planning request. Coalesce automatic changes for three
hours after a request; new operator direction may bypass that interval. Ignore
the controller's own comments/timestamps as change triggers. Do not repeatedly
plan an unchanged empty epic. A planning run in flight holds that epic. A plan
must still match its requested intent and target preconditions; drift requires
a new plan, not overwriting changed child text. Read Priority values from the
project, never from a second configured list. New children default to its last
Priority option (the lowest priority); a plan's explicit set_priority overrides
that. Use the same derived default when verifying recovered create effects.

Use the existing dispatch record to identify the planning request/run and the
accepted epic plan's comment ID and change time to identify the answer. Pin
request, comment and change time when applying. No separate `coordinator/v1`
pending checkpoint may override the forge evidence. A completed run gets five
minutes for its answer to arrive; an unconfirmed dispatch gets six hours before
being reported unresolved. Neither starts an automatic retry. New operator
direction or a replacement plan is the recovery path.

On every pass, examine the trusted `agent-board-epic-apply: ID@TIME` receipt
**before** deciding that a plan is still waiting. A claim means “do not replay,”
not “all actions succeeded.” Inspect its confirmed writes and refresh the
census. A successful dated apply result plus its
`agent-board-epic-fingerprint` records the post-apply membership/state baseline;
those writes are not fresh demand for planning. That fingerprint does not cover
body or Priority: retain/compare those facts separately from the resolved
request and actual effects.

If the final status was lost, establish completion only by checking every
action's effects against confirmed identities in the receipt and current
complete facts (including board membership, text, Priority, labels and child
plans as applicable). Do not identify a created issue solely by title. If all
effects are proved, resolve the request and use the fresh post-apply baseline.
If partial, ambiguous or unreadable, report the exact unresolved actions for a
person; retain the claim and never replay it. In particular, `Already claimed;
nothing changed.` is not success evidence, and stale local pending state cannot
freeze an epic whose apply is already proved. This observation-based recovery
is part of the first implementation, not deferred cleanup.

## One configuration file

The proposed JSON schema below is exhaustive; reject unknown keys and nulls.
Required means no default. Optional sections are absent/off unless stated.
This is a new pass schema, not automatic acceptance of the old caller file.
The pinned executable/build reference belongs in the workflow, not this file.

| Key | Meaning; default |
| --- | --- |
| `operator`, `bot` | Direction/plan writer and applying bot logins; required |
| `dispatchRepo`, `dispatchWorkflow`, `dispatchRef` | Caller repository/workflow/ref; required, required, caller default branch |
| `follow` | Exact repositories for existing PR follow-up; `[]` |
| `workRepositories` | Exact allowed planning/implementation repositories; `[]` |
| `board.owner`, `board.project` | Project owner and positive project number; required when work is enabled |
| `board.priorityField` | Priority single-select field; `"Priority"` |
| `epics.repository` | Repository containing live epics; absent disables epic planning |
| `wip` | Total active-run limit, integer 0–100; `0` (new work off) |
| `triage` | Permit no-plan triage; `false` |
| `planPools`, `triagePools` | Ordered pool names for epic planning/no-plan triage; `[]`, `planPools` |
| `pools` | Map of automatic pool names to pacing settings; `{}` (no automatic runs) |
| `pools.NAME.target`, `pools.NAME.burst` | Fraction of paced window to spend (0 < target ≤ 1), burst percentage points (0–100); `0.95`, `0` |
| `usage.url` | Broker usage endpoint; required to refresh when pools are configured |
| `usage.repository`, `usage.issue` | Private snapshot destination; required when pools are configured; writer is `bot` |
| `state` | Optional disposable poll-cache path; absent means fresh reads |
| `pr.reviewWorkflow` | Existing trusted review workflow; absent counts only operator reviews |
| `pr.fixPools` | Ordered fix-round pools; `[]` leaves caller default |
| `pr.toilFixPool`, `pr.conflictFixPool` | Special fix pools; absent uses ordinary fix choice |
| `pr.mergeAuthors` | Extra authors eligible for existing green-check merge path; `[]` |
| `pr.rebaseRounds` | Separate conflict-only round cap, 0–3; `0` charges normal fixes |
| `pr.mergeQueue.tries` | Opt into existing merge-queue path with bounded attempts, 1–5; section absent/off, present default `3` |

The PR stage otherwise uses today's default poll policy, including two fix
rounds and existing default continuation handling. Its runs use the same inline
pool definitions for pacing: previous usage in stage 1, refreshed usage for
later starts; no tiers or second pools file.
No additional poll optional switches are exposed in this initial pass schema;
standalone `poll` remains available. Credentials and transport-specific provider
normalization are not per-deployment config keys. The caller enables new work by
setting `workRepositories`, `wip`, pools and usage; triage additionally needs
`triage: true`, epic planning `epics.repository`. The cutover PR must supply those
settings explicitly; merely upgrading the binary enables none of them.

## Replay and release gate

Record sanitized, complete live facts with injected clock and expected actions,
holds, writes and dispatch count; replay the contract, not every JavaScript unit
test. Include idle/free-slot boards on **22 and 23 October**, frozen epics with
completed applies but stale pending state, partial/ambiguous applies, overlap
behind rescue drafts, under-pace fill opportunities and over-pace pools. Also
exercise full WIP, missing/stale usage, invalid latest plans, incomplete pages,
lost dispatch responses and reruns after each write boundary. Reruns must never
duplicate creates or dispatches. Document intentional differences under the
approved drop list; zero dispatches on an idle board with eligible free slots
and usable pools is a regression, not success.

Before production, run preview then apply in a separate staging organization
with isolated repositories/project and credentials. Verify actual receipts,
recovery after interruption, capacity and overlap holds, and a second idle pass;
compare the recorded action trace with replay expectations. This document does
not claim those fixtures or the staging run have happened.

Cut over only after approval and that gate. One caller PR replaces the old
`poll.yml` jobs with a cron, serialized apply job, pinned agent-board build and
`agent-board pass --config … --apply`; delete the replaced scripts, their tests
and Justfile recipes, and disable their wake/chain triggers. No dual apply
actors. Move supervisor/release-train starts to their own crons first, preserving
their independent authorization. Keep existing receipts; optional caches may be
discarded. Rollback is reverting that single caller PR and stopping any queued
new pass before restoring the old actor; inspect ambiguous writes, never erase
claims to force a retry.

**Deployment examples only:** the source audit below is from
`cgwalters-forge/tracker` main at `1a550cc7d5089b30b5bd1e383a2efc7a9c27759c`;
the required staging deployment for that caller is `cgwalters-forge-stage`.
Neither name is a default or an agent-board policy.

## Drop list for approval

These are deliberate removals from the audited caller, not implied parity.
Links are immutable source locations; retained semantics are described above.

* [Fill mode, issue-page/candidate caps and accelerated planning][fill]: used
  spare pace to scan outside epics; one complete board candidate set and one
  scheduling rule remove starvation and the second set of limits.
* [Requested items][requested] and [enqueue][enqueue]: reordered work, wrote
  Priority/Todo and woke poll; use ordinary board Priority and the next cron,
  with no `items` bypass or `just enqueue` recipe.
* [Operator retry pools][operator-retry] and [recorded-plan outcome retries][retry]:
  retried failed runs, inferred blockers, closed merged outcomes and escalated
  exhaustion; retain receipts, but require a replacement plan/person instead
  of another retry state machine. GitHub closing keywords remain effective.
* [Scheduler triage verdicts, short-body heuristic and lost-question retries][triage]:
  guessed done/design from body length and answers, closed issues, retried then
  escalated; triage now only proposes a plan/hold, once per recorded request.
* [Automatic batching and batch-release probes][batch]: bundled sibling plans
  and recycled extras; one issue per scheduled run makes capacity and recovery
  unambiguous. Existing standalone dispatch batching is not removed.
* [Coordinator checkpoints, daily UTC boundary, failure counters and empty retry][checkpoint]:
  maintained a second pending/applied history; existing request/apply receipts
  plus observed effects replace it, elapsed intervals replace the wall-clock
  boundary, and unresolved runs require inspection rather than three retries.
* [Model-written status briefs/project health updates][status]: sampled PR/run
  history and published prose; not needed to start work or recover applies.
  Keep factual pass diagnostics, not a second reporting agent.
* [Title-based missing-effect matching][effects]: inferred created identities
  after a retained claim; use confirmed receipt identities or stop as ambiguous.
* [Approved-plan assignment/label choreography][approval]: special-cased an
  assigned needs-human plan and removed its label; explicit human removal of
  holds and operator epic designation suffice, with no implicit hold removal.
* [Broker probe/outage-issue lifecycle and global preview hold][broker]: opened,
  refreshed and closed outage issues, held all actors; fresh usable usage and
  ordinary dispatch failure reporting suffice for this pass. Outage monitoring
  belongs outside scheduling; no outage-based fix-round refunds are configured.
* [Encrypted private Decisions baseline][decisions]: read/compared private
  answers on every poll; separate operator interaction, not a scheduling input.
* [Supervisor starts][starts] and its [due][due], [profile][profile] and
  [repeat-failure][repeat] helpers: woke/paced a separate health actor and packed
  operator tasks; relocate with that actor's own cron, not into pass.
  [Release-train starts][starts] likewise move to its own cron.
* [Comment wakes][wake], [completion wakes, successor chain and dual jobs][workflow]:
  reduced cron latency but created several clocks and capacity races; one cron
  and serialized pass suffice. Remove separate BOARD_DISPATCH/COORDINATOR modes,
  stderr parsing and inter-job dispatched-item files.
* [Branch-head builds and revision-keyed artifact restore/backoff][workflow]:
  rebuilt twice and restored cached state to sustain a chain; pin once, make
  cache optional, and let the caller cron supply cadence. Never restore fork
  artifacts as trusted cache if a caller later adds persistence.
* [Usage freshness-only workflow wake][usage-refresh], [first-page publication
  restriction][publish] and [provider cost/empty machine meters][meters]:
  skip/wake optimization and UI compatibility fields are not scheduler rules;
  refresh inline, locate the trusted comment with bounded complete reads, omit
  cost estimates and machine-only meters. Keep private-only publication,
  authenticated writer, paced-window/burst/exhaustion calculation and original
  timestamps. Do not carry an old paced reading as proof that a missing short
  window is safe; a restart without current hold facts waits.
* [Old caller configuration][config]: remove source/branch, coordinator/status,
  fill/batch/retry/tier knobs and the separate usage/pools config files. The
  initial pass also does not expose optional standalone-poll operator-comment
  relays, notification subscriptions, operator-answer label clearing,
  collaborator locking, admission envelopes, diagnostic/continuation overrides
  or custom attribution. Those commands/features remain independently usable;
  adding them to pass later requires a demonstrated need, not silent migration.

[fill]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/poll/schedule.cjs#L572-L659
[requested]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/poll/schedule.cjs#L693-L774
[enqueue]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/poll/enqueue.cjs#L88-L142
[operator-retry]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/poll/schedule.cjs#L467-L538
[retry]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/poll/schedule.cjs#L890-L929
[triage]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/poll/schedule.cjs#L813-L848
[batch]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/poll/schedule.cjs#L336-L385
[checkpoint]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/poll/coordinator.cjs#L107-L184
[status]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/poll/coordinator.cjs#L450-L519
[effects]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/poll/coordinator.cjs#L335-L380
[approval]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/poll/epics.cjs#L46-L72
[broker]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/poll/broker.cjs#L14-L100
[decisions]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/poll/decisions.cjs#L9-L78
[starts]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/workflows/poll.yml#L171-L191
[due]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/poll/supervisor-due.cjs#L4-L49
[profile]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/poll/supervisor-profile.cjs#L5-L68
[repeat]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/poll/supervisor-repeat.cjs#L4-L48
[wake]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/poll/comment-wake.test.cjs#L13-L65
[workflow]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/workflows/poll.yml#L8-L266
[usage-refresh]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/usage/usage.cjs#L232-L264
[publish]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/usage/usage.cjs#L266-L295
[meters]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/usage/usage.cjs#L123-L187
[config]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/poll/agent-board.json#L1-L62
