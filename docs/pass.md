# One pass

This is the proposed contract for `agent-board pass`; the operator reviews it
on the pull request that changes it, and the [architecture](architecture.md)
records the decisions it follows. A pass
finishes once; it is not a daemon, a second planner, or a JavaScript compatibility
layer. Existing [PR follow-up](poll.md),
[job-plan](job-plans.md) and [epic-plan](epic-plans.md) protocols remain the
authority for their operations. The differences below are intentional.

## What is built

The pass implements the configuration below and stages 1 to 5, except
that there is no infrastructure rerun, and stage 2 still writes poll's claim
and marker comments in every followed repository, external ones included.

Stage 1 reads `usage.url`, the broker's `/usage`, without the forge token or
an environment proxy (plain HTTP is allowed for a broker on a private network
that encrypts it, such as a tailnet), and paces each configured pool it reads
(claude from Anthropic's windows, openai from Codex's) as tracker's
[`usage.cjs`][meters] does. Its line is
`usage: claude: 18% used, pace allows 50%, resets T, read T; openai: …`, and
stages 2 to 4 pace by it, pull requests' fix and continuation runs included.
A preview then prints
`usage: publish to OWNER/REPO#N; a preview paces by it unpublished`; an
applying pass edits the bot's comment
there (or adds one, as the bot), only in a private repository, and prints
`usage: published URL`. That is one write step, and the pass's first,
so `--max-writes 1` spends it there. A failed read is one line,
`usage error: reading the broker: …`, counted as a failure, and pacing falls
back to the published snapshot under the staleness rules below. Without
`usage.url` the line says pacing reads the published snapshot. Unlike
`usage.cjs`, a pool the broker has no current week for (after a broker
restart) is null, so held, not carried from the previous snapshot; and the
snapshot has no `windows`, `workers` or Mistral allowance (the review app
reads that from the pools file). A recording keeps the broker's answer under
no address.

The broker learns a pool's windows only from the responses to requests made
through it, so after a restart a held pool would never get a reading. With
`usage.probe`, a pool with no reading whose count of requests in the broker's
answer (`anthropic.counts`, `codex.counts`, as `mistral.counts`) is zero, and
with no run in flight named for it, takes one run in stage 4, within
`--max-dispatch`: `probe: claude has no reading; one run to take one` before
its start. Its requests give the next pass a reading. A pool with requests and
still no reading, or without a count, stays held, and the published snapshot
holds it either way (`no-reading`).

With `budget.weeklyUsd` set (agent-board#281), stage 1 also records the
broker's `spend.runs`, the trailing week's cost of runs' requests at API list
prices in dollars ([praxis-credential-broker#38][spend]), in the snapshot's
`budget` (`weekly_usd`, `spent_usd`, `prices`, `unpriced_requests`, and the
broker's `since` and `unmetered_requests` where it gives them), and its line
ends `budget: $412 of $1000 this week at list prices`. Spend in another
currency, basis or window is no reading, and so is a broker without `runs`, a
total that leaves runs' requests unpriced
(`budget: $1000 a week, 3 request(s) unpriced (held)`) or does not say how many
it does, and a snapshot without it: never $0. Operators' interactive use
neither counts nor, unpriced, holds. A `since` under a week old does not hold
either: the broker's spend restarts with it until praxis-credential-broker#35,
so the week may read low after a deploy. Once the week's spend reaches the budget,
stages 3 and 4 start no new run (`budget: $1003 of $1000 this week; new runs
wait`); pull requests' fix and continuation rounds go on until it is 10% past,
so that work in flight can land, then wait too (`…; pull requests' fix rounds
wait`). A missing or stale spend reading holds both, as missing usage does.
Without `budget` nothing holds on spend.

Stage 3 reads the open `epic` issues of `epics.repository` with their
sub-issues (20 epics a request), the board's Priority options, and each live
epic's comments, and takes the live epics in Priority order. Its first line
counts them, such as `epics: 7 live of 17 open in example/tracker`, then
one line an epic:

* `apply epic OWNER/REPO#N plan=ID: Create a sub-issue in example/work (P0);
  Close example/work#9 as not planned` where the newest accepted plan has no
  receipt, applied with `epic apply`'s checks, its comment and change time
  pinned. A new sub-issue gets its final Priority with its create: the plan's
  `set_priority` of it, else the board's last option. An applying pass ends
  the line with what the [receipt](epic-plans.md#apply) says and
  `receipt=URL`. Its receipt and each action that writes are one write step
  each, held whole by `--max-writes`; one epic is applied a pass.
* `recover epic …` where the receipt was cut short (`claimed` or `stopped`)
  and every action's effect is proved by the identities it confirmed and the
  epic now; an applying pass rewrites the receipt as `recovered`, one write.
  Writes after a create that the receipt did not confirm are observed: the
  sub-issue's link, its board Priority and its job plan. One that is not
  proved is held for a person with the actions it cannot prove, and never
  replayed; so is an epic with a receipt that cannot be read.
* `start planning OWNER/REPO#N pool=P: why` for the first due epic: a
  triage run on the epic, started as stage 4 starts one, from `planPools`,
  whose brief asks for an epic plan. It takes a slot from stage 4 and counts
  against `--max-dispatch`; one epic is planned a pass.
* `epic OWNER/REPO#N: plan ID applied; nothing changed since T`, or
  `hold epic OWNER/REPO#N: why`: a planning run in flight, a request waiting
  for its plan or answered by none, an invalid newest plan, or a person's
  hold (`needs-human`, `needs-design`, board Blocked), which also says what
  the epic would otherwise come to.

An applied plan is recognised from its receipt, older receipts (the hidden
claim and fingerprint comment) included, never from titles: a sub-issue
retitled, closed or reprioritised since is not missing. An epic is due when
it has no plan or request; on the operator's comment since its last plan
(for a plan that cannot be applied or recovered, since its last request too,
so a run that answers nothing is not started again);
otherwise, once three hours have passed since its last request (its record,
or for an epic planned before this stage, its plan), when the epic's text was
edited, a sub-issue was added, removed, closed or reopened against the
receipt's fingerprint, a sub-issue's text was edited, or a Priority the
apply set differs now; and a day after its last plan, unless nothing is open.
Not yet noticed: a Priority change of a sub-issue the apply did not set,
until that day's replan.

Stage 5 reads the whole board (closed issues and pull requests included, in
about one request per hundred items) besides the runs and open issues that
scheduling reads, and reuses those where scheduling read them. Its first
line counts the changes, such as
`status: 39 change(s) of 46 item(s) read; 2 kept, their own facts not read whole`
(draft issues and redacted items, which the whole-board read cannot see,
are not counted and keep their Status),
then one line per change, `status OWNER/REPO#N: OLD -> NEW (why)`, for
example `status example/work#12: Todo -> In Progress (pull request
example/work#14 is open)`. Each change is one write step; one that
`--max-writes` holds starts `held by --max-writes: `. An applying pass
reads the item's Status again just before writing it, and leaves one a
person changed since (`changed to X since the board was read`); that read
and the write are two of the pass's 500 requests, so once the budget or the
time bound is spent the rest wait for the next pass, on one line. The stage
is not serialized by the hold that keeps two passes from starting work: two
overlapping passes derive the same values, and the read before each write
leaves what the other wrote. More changes than `--max-status-changes` (50
unless given) refuse the stage whole: it writes none, prints each as
`refused: status …`, and fails the pass, a preview too, so that a first
pass over a board that drifted is looked at first and then let through
once with a higher number.

The replay gate below is partly met: there is one recorded board, from 23
October, with a synthetic usage snapshot and an empty whole-board read, and
stage 3's reads of the epics tracker#596, #766 and #890 that evening appended
to it. Scenario tests edit that recording; on it #596, frozen by a title
match until tracker#851, is planned, and #890's applied plan is not applied
again. Stage 5 has its own recording of the
same day's board, trimmed to items that show each rule
(`fixtures/pass/status-2026-10-23.json`). No staging run has happened.

Try a pass without credentials, against the recorded board:

```sh
agent-board pass --config fixtures/pass/config.json --replay fixtures/pass/board-2026-10-23.json
```

Each decision is one line: `start triage|implement|planning OWNER/REPO#N … pool=P`,
`apply|recover epic OWNER/REPO#N …`, `epic OWNER/REPO#N: …`, `usage: …`,
`hold [epic] OWNER/REPO#N: why`, `hold wip|pools: why`,
`held by --max-dispatch: start …`, `status OWNER/REPO#N: …`, `fail …` or
`… error: …`; a pull request's
step that a cap held ends `-> held by --max-dispatch` or
`-> held by --max-writes`. With `--apply`, a started line also names its
dispatch record and run. The last line is a summary such as
`summary: dispatched 1/1, writes 3/5, held 12`, which counts what a preview
plans and what an apply did, and the steps the caps held; an uncapped kind
has no `/N`.
`--record FILE` previews live and saves every read as a new fixture. Add
`--replay SEED` to answer reads the token cannot make, such as a private usage
snapshot or the broker's `/usage`. A preview counts only the PR runs that stage 2 actually starts, so it
can show more free slots than an applying pass would.

A triage run is started by posting the bot's `triage` job plan on the issue and
dispatching it with the job-plan protocol, so the dispatch record is the claim.
A pass interrupted between that post and its dispatch leaves a triage plan,
which is a manual dispatch; a person dispatches or replaces it.

In a GitHub Actions run, stage 4 of an `--apply` pass starts nothing while
another run of the same workflow is in progress (`hold wip: another pass is in
progress`): two passes that both saw a free slot could otherwise both take it.
Give the caller's workflow a `concurrency` group, so that passes queue rather
than hold each other. Outside Actions, `--apply` is refused unless both
`--max-dispatch` and `--max-writes` are given, and the pass prints a warning
that it is not serialized against the scheduled workflow: a capped local apply
is how a person tries new write behavior on a live board one step at a time,
for example `--max-dispatch 1 --max-writes 3`, and the caps bound what a race
with the scheduled pass can cost. Both stages start runs from `dispatchRef`, or the caller's
default branch. Past 15 minutes no new request is sent; writes, and a start that
passed its last check in time, have two minutes more to finish, after which
what was written is observed by the next pass.

## Order and effects

`agent-board pass --config FILE` previews by default. Only `--apply` permits
writes, and then pass makes them itself with the bot's token: Status, labels,
comments, closes, merges and dispatches. These are deterministic consequences
of forge facts, not agent output, so they do not go through agentic-job's
check and apply; that boundary is for what agents produce. Their guards are
preview by default, caps per pass, and the replay gate below. The caps are
two flags, unset by default: `--max-dispatch N` bounds the agent runs a pass
starts, stage 2's fix, rebase, continuation and answer rounds, stage 3's
planning run and stage 4's starts together, and `--max-writes N` bounds its
other write steps, forge writes such as comments, labels and merges (stage
1's usage comment, stage 2's comments, labels, retitles, retargets, enqueues,
merges and branch updates, stage 3's
epic apply, a step for its receipt and each action that writes, taken whole,
and its recovery, and stage 5's Status changes).
The unit is a step, not a request: a fix round's claim, marker and dispatch
are one dispatch, and a stop's label and marker one write. A step is checked
before its first write and taken whole or held whole; a held step is
reported on its line and left to the next pass.
A held start takes the slot it would have had, so a capped pass reads no
further than an uncapped one. The numbers for the scheduled pass are an
[open question](architecture.md#open-questions).
The caller serializes apply passes for the board, except a capped local apply,
which is not serialized against the scheduled pass and is bounded by its caps
instead. It uses trusted default-branch configuration and a pinned build, and
supplies credentials through the environment, never the file. Issue text, PR
bodies, run names and comments other than authenticated operator direction are
data, not instructions.

`--fix-bounds FILE`, absent by default, names the caller's agentic-job bounds
file for its `fix` kind (tracker's `.github/agentic-job/fix.toml`), from the
same trusted checkout as `--config`. With it, stage 2 starts no fix, rebase
or continuation round, all dispatched as that kind, on a pull request
whose repository or head branch the file's `repos` and
`outputs.push_to_pull_request_branch.branches` leave out; its line says
`skip: left to the operator: head branch "B" is outside the fix bounds`, and
nothing is claimed, marked, labelled or dispatched. Merges are unchanged.
The [poll](poll.md) says which globs it reads. It does not replace
agentic-job's policy step, which stays the boundary; it saves the run that
step would refuse.

A pass captures one clock and performs these stages in order:

1. Refresh usage from the configured broker, retaining the existing normalized
   paced-window calculation (target, burst, short-window exhaustion and original
   observation time). Read the previous trusted snapshot and publish the new
   `bot-usage/v1` snapshot to the configured private issue. Preview computes it
   without publishing, and paces by it all the same. A failed refresh is
   visible; it cannot turn missing or stale readings into spare capacity.
2. Follow up PRs using today's `poll` decisions: read followed repositories,
   PR heads, reviews, checks, comments and runs; dispatch bounded fixes/reviews,
   merge only an approved head with green required checks, or comment/label for
   a person. Retain its claim and head-recheck rules, but follow the
   [bookkeeping rule](architecture.md#bookkeeping): in an external repository
   the claims and markers become one hidden comment the bot edits in place, and
   it posts no claim or status comments there. Count every dispatch made
   here against capacity before starting anything later. Pacing here reads
   stage 1's reading, or where the refresh failed or is not configured, the
   previous trusted snapshot; missing/stale readings hold new PR runs,
   including on a cold start.
3. Read the complete board, Priority options, live epics, their children,
   authenticated operator comments, accepted plans, apply receipts and current
   runs. Recover completed epic applies, apply an accepted pending epic plan,
   or start at most one due epic-planning run. Epic apply may create/link issues,
   set Priority, rescope or close children and post plans/receipts within the
   existing epic bounds; planning itself only dispatches a triage run. Refresh
   affected board/issue facts after an apply before scheduling.
4. Rerun, once, each scheduled run that failed for an infrastructure reason,
   then choose and dispatch work by the rule below. Dispatch writes the existing
   plan claim and run link, and the existing board Run update where supported;
   it does not close issues from guessed outcomes.
5. Derive Status for every board item, closed issues and merged pull
   requests included (this stage reads those, which earlier stages skip),
   from this pass's facts and its own starts, and write each one that differs:
   * closed issue, or merged or closed pull request: **Done**;
   * otherwise, the `needs-human` label on the item: **Needs human**;
   * otherwise, a run in flight for it, or an open pull request for it (the
     item itself, or one that says it closes the issue, in any repository and
     by any author; draft or not): **In Progress**;
   * otherwise, a derived value that no longer holds (Needs human, In
     Progress, or Done on an issue open again) goes back to **Todo**;
   * otherwise the Status stays. **Blocked** and **Backlog** are a person's
     holds: only Done replaces them, so neither the label, a run nor a pull
     request moves them (the label still holds scheduling). The legacy
     columns (Triage, Draft, In Review, Cancelled) give way to any rule
     above; a person's **Todo** stays.

   The rules' precedence is shared with poll's `statusReconcile` preview.
   No person or agent sets the derived values, `agent-board ask` included,
   which only sets and clears the label; an edit to one is overwritten on the
   next pass. An item whose facts were not read whole keeps its Status:
   its own fields, labels or state incomplete, or, for the fall back to Todo,
   runs that could not all be listed or an open issue's closing pull requests
   that could not all be read.

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
repositories, excluding epics and the usage issue. An epic child qualifies
only through a live epic whose latest `epic` label event is the operator's.
A live epic is in `epics.repository`, or with `epics.inWorkRepositories` in
any work repository; stage 3 plans only the former.
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
without a usable plan remains visible for a person; do not automatically retry,
unless `triageFollowUpDays` is set. Then the triage brief asks for a
`schedule-triage` verdict, and once the run is no longer in flight the pass
reads the newest one the bot posted after the dispatch record, unedited but by
a writer; a visitor's comment is never a verdict. A comment by the operator
after it (or after the record, with no verdict) triages the issue again.
Otherwise `done`, `needs-design` and `needs-operator` label it `needs-human`,
one write step, so it shows as the operator's ask; the pass never closes an
issue on a verdict, since no review follows a close. Any other verdict, or
none, triages the issue again `triageFollowUpDays` after the verdict (or the
record): at most one triage run per issue in that many days.

With `bodyPlans` (agent-board#342), an issue in the board's first two Priority
options (P0 and P1) that has no plan comment is first read for a plan in its
own body: one `agent-board-plan` block, weighed as a plan comment is. The
body must be the operator's or the bot's, if edited last edited by one of
them, and its plan must pass the plan checks. Such a plan is treated as the
issue's accepted plan, with the same holds, target repository and pool checks.
At its start the pass reads the body and comments again, and holds the issue
if the body changed or a plan comment appeared. It then posts a copy as the
issue's plan comment and dispatches and records that copy through the plan
protocol, only while it is still the newest plan, so it runs once; from
then on the copy is the plan, and a later edit of the body changes nothing. A
body with no such block, or another author's, leaves the issue to triage. A
body plan that is invalid, or that someone else edited last, holds the issue.
An epic plan may not put a plan block in a body it writes.

The one automatic rerun is for infrastructure: a scheduled run that failed
before the agent's work could succeed or fail on its own (runner lost, setup
or checkout failed, broker or forge outage; the exact rule is an
[open question](architecture.md#open-questions)) is dispatched again once,
with the same plan and pool choice rules, counted against WIP and pacing like
any start. The rerun is recorded on the same dispatch record, so a second
infrastructure failure, or any failure of the agent's own work, is left for a
person.

With an accepted implement plan, dispatch precisely that plan once. An invalid
newest plan does not mean “no plan”; report it, never fall back. Recorded plans
do not run again, except the infrastructure rerun above; an operator/planner
edit or replacement is needed. Research or
triage plans explicitly posted on issues remain manual dispatches.

Choose among the configured automatic pools by greatest room below pace:
`target_percent * elapsed_fraction - used_percent`; ties use config order.
Exclude held, exhausted, reset, missing or stale readings (snapshot older than
45 minutes or paced observation older than 30 hours), and readings more than five
minutes in the future. If none qualifies, wait;
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

An epic's repository is first-party, so its apply receipt follows the
[bookkeeping rule](architecture.md#bookkeeping): one comment on the epic, the
claim posted before any write and updated as writes are confirmed, readable
first. It has one line per sub-issue saying what the apply did to it, for
example `- Created agentic-job#715 (P0)` or `- Closed #611 as not
planned`, then one fenced
`agent-board-epic-apply` JSON block with the machine detail: the plan comment
ID and change time it claims, each confirmed action with the identity it
produced, the outcome, and the post-apply fingerprint. It replaces the hidden
`agent-board-epic-apply` claim and `agent-board-epic-fingerprint` markers, which
are still read on older epics.

On every pass, examine the trusted receipt **before** deciding that a plan is
still waiting. A claim means “do not replay,”
not “all actions succeeded.” Inspect its confirmed writes and refresh the
census. A successful dated apply result plus its
fingerprint records the post-apply membership/state baseline;
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

## Budget tree

`agent-board budget --config FILE` reads the pass configuration and prints
what work has cost so far (agent-board#281). The first line is the board, with its
`budget.weeklyUsd`. Below it come the open epics of `epics.repository`, then
each epic's sub-issues, nested as deep as GitHub nests them. Each item shows its
`Budget ($)` number field on `board` where one is set, and its actual: the cost
of its own runs plus that of its descendants. An epic that sits under another
epic appears only there. The command is read-only and holds nothing.

A run's cost is the `<!-- agentic-job-cost: RUN usd=N partial=B -->` line that
agentic-job (agentic-job#1945) writes into the run's status comment on its
target. It is the list price in dollars. An item's own runs are those on the
item and on every pull request that closes it (GitHub's closing references,
closed pull requests included), so its implement, fix rounds and reruns all
count. A line counts only if it stands on a line of its own, in a comment by
`bot` that nobody else edited last, and that comment also carries the same
run's `<!-- agentic-job-run: RUN -->` line, as agentic-job's status comment
does. Anyone can comment on a public issue, so this keeps a pasted line from
moving an actual, and the run marker keeps a cost line copied into the agent's
own result comment from counting; it does not stop an agent that writes both
lines on purpose. Each run counts once in the whole tree, at the
first item that names it in the order shown: a pull request that closes two
sub-issues adds its runs to the first of them only. Each actual is a lower
bound, and an `at least` marks one that includes a partial cost. Some costs are
left out without an error: a run with an unknown cost leaves no line, a status
comment written as `github-actions` is not `bot`'s, a rerun that edits a single
marked status comment replaces the line before it, and a pull request that does
not say which issue it closes is not read. Actuals cover all time, not the
week. What it reads, it reads whole: it refuses more than 100 open epics or 300
items, 1000 requests, 1000 comments on an item or pull request, a sub-issue
list, a list of more than 20 closing pull requests or a set of boards that
could not be read whole, and a `Budget ($)` value that is not a dollar amount.

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
| `epics.inWorkRepositories` | Also take operator-labelled epics in every work repository as live, for candidates only; `false` |
| `wip` | Total active-run limit, integer 0–100; `0` (new work off) |
| `triage` | Permit no-plan triage; `false` |
| `planPools`, `triagePools` | Ordered pool names for epic planning/no-plan triage; `[]`, `planPools` |
| `triageFollowUpDays` | Act on an ended triage's verdict and triage again after this many days (0–30), see [scheduling](#scheduling); needs `triage`; `0` (off) |
| `bodyPlans` | Start a P0 or P1 issue with no plan comment by the plan in its body, see [scheduling](#scheduling); `false` |
| `pools` | Map of automatic pool names to pacing settings; `{}` (no automatic runs) |
| `pools.NAME.target`, `pools.NAME.burst` | Fraction of paced window to spend (0 < target ≤ 1), burst percentage points (0–100); `0.95`, `0` |
| `usage.url` | Broker usage endpoint; required to refresh when pools are configured |
| `usage.repository`, `usage.issue` | Private snapshot destination; required when pools are configured; writer is `bot` |
| `usage.probe` | Let a pool with no reading and no request since the broker started take one run a pass; `false` |
| `budget.weeklyUsd` | Weekly spend at API list prices, in dollars, at which new runs wait (0 < budget ≤ 100000); needs `usage.url`; absent holds nothing on spend |
| `state` | Optional disposable poll-cache path; absent means fresh reads |
| `pr.reviewWorkflow` | Existing trusted review workflow; absent counts only operator reviews |
| `pr.fixPools` | Ordered fix-round pools; `[]` leaves caller default, unpaced, so required when `follow` and `usage` are set |
| `pr.toilFixPool`, `pr.conflictFixPool` | Special fix pools; absent uses ordinary fix choice |
| `pr.mergeAuthors` | Extra authors eligible for existing green-check merge path; `[]` |
| `pr.rebaseRounds` | Separate conflict-only round cap, 0–3; `0` charges normal fixes |
| `pr.mergeQueue.tries` | Opt into existing merge-queue path with bounded attempts, 1–5; section absent/off, present default `3` |
| `pr.staleDays` | Act on a stopped bot pull request the operator left alone this many days, 0–30, see below; `0` (off) |

The PR stage otherwise uses today's default poll policy, including two fix
rounds and existing default continuation handling. Its runs use the same inline
pool definitions for pacing, by stage 1's refreshed usage like later
starts; no tiers or second pools file.

`pr.staleDays` (agent-board#325) ends the hold a stopped pull request keeps
on every item that overlaps it. A pull request of the bot's from its fork
that the poll labelled `needs-human`, and that conflicts with its base or has
changes requested (the operator's, or the review job's by its run), is stale
once the poll's stop is that many days old and the operator has not acted
since: no comment, review or review comment of theirs with text, no new head,
and the label's newest `labeled` event still the bot's. If no fix round ran
before the stop, it gets fix round 1, claimed from the stop and started as
the round on an operator's answer is (`fix: … so it gets a fix round`): the
label comes off once the run is recorded, and the old head's approval
cannot merge it until the round succeeds with a new head. Otherwise the poll
reads the label, head and operator's comments again, claims the close from the
stop as a reset or answer round is claimed, posts a new stop saying why and
closes it (`close: … so it is closed`), one `--max-writes` step; reopened, it
is left for another `staleDays`. A pull request with more reviews, checks or
labels than one query reads is left alone, as the poll waits on one otherwise. A round
on a stale stop counts as fix round 1, so a pull request gets at most one
such round before it is closed, unless a person resets it. The round
respects `--fix-bounds`, `--max-dispatch` and pacing like any other.
No additional poll optional switches are exposed in this initial pass schema;
`poll` becomes this stage rather than a separate command, so the switches
not listed here go with it at the cutover unless a need adds them.
Credentials and transport-specific provider normalization are not
per-deployment config keys. The caller enables new work by setting
`workRepositories`, `wip`, pools and usage; triage additionally needs
`triage: true`, epic planning `epics.repository`. The cutover PR must supply
those settings explicitly; merely upgrading the binary enables none of them.

## Replay and release gate

Record sanitized, complete live facts with injected clock and expected actions,
holds, writes and dispatch count; replay the contract, not every JavaScript unit
test. Include idle/free-slot boards on **22 and 23 October**, frozen epics with
completed applies but stale pending state, partial/ambiguous applies, overlap
behind rescue drafts, under-pace and over-pace pools, and Status derivation
on every rule above. Also exercise full WIP, an infrastructure failure rerun
once and a second one left for a person, missing/stale usage, invalid latest
plans, incomplete pages, lost dispatch responses and reruns after each write
boundary. Reruns must never duplicate creates or dispatches. Document
intentional differences under the drop list; zero dispatches on an idle board
with eligible free slots and usable pools is a regression, not success.

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

## Drop list

These are deliberate removals from the audited caller, not implied parity;
the operator decided the list on 23 October 2026
([architecture](architecture.md#reruns-and-the-drop-list)). Everything here is
dropped except the one infrastructure rerun kept from the retry entry.
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
  Kept: one rerun of a run that failed for an infrastructure reason
  ([scheduling](#scheduling)), not of an agent's failure.
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
  ordinary dispatch failure reporting, plus the one infrastructure rerun,
  suffice for this pass. Outage monitoring
  belongs outside scheduling, in the [broker probe](broker-probe.md); no outage-based fix-round refunds are configured.
* [Encrypted private Decisions baseline][decisions]: read/compared private
  answers on every poll; the decisions document folds into the `needs-human`
  label, which is already a scheduling hold.
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
  or custom attribution. With `poll` folded into pass they go at the cutover;
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
[spend]: https://github.com/cgwalters-bot/praxis-credential-broker/issues/38
[meters]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/usage/usage.cjs#L123-L187
[config]: https://github.com/cgwalters-forge/tracker/blob/1a550cc7d5089b30b5bd1e383a2efc7a9c27759c/.github/poll/agent-board.json#L1-L62
