# Pruning the backlog

> This text is LLM generated.

`agent-board prune --config FILE` lists the open issues of the pass
configuration's `workRepositories` that are candidates to close, each with its
reasons and the links that show them (agent-board#280). It is read-only: it
doesn't comment on, label or close issues. Closing issues, capping those writes
and adding a schedule are future work; this list is for the operator to review first.

The configuration is the [pass's](pass.md#one-configuration-file), from a
trusted default-branch checkout. Prune reads its `workRepositories`, its
`board` (for each issue's Priority, from the board's Priority field, which
must exist and be single-select: otherwise the read is refused rather than
every issue taken as unprioritized) and its `bot`. `GH_TOKEN` reads; it needs read access to the repositories' issues and
to the project.

## Reasons

An issue labelled `keep` (in any case) is skipped whole and only counted.
Pull requests are left out. Any other open issue is listed with each reason
that applies:

- **overtaken**: a merged pull request of one of the `workRepositories` is
  among those GitHub takes as closing the issue (its closing references, as
  a closing keyword or the Development sidebar make them). The evidence is
  that pull request. A cross-reference alone does not count, as its text
  may say anything ("this does not fix …"), nor does an issue closed as
  completed, a pull request closed without a merge, or one from another
  repository.
- **duplicate**: an older open issue of the same repository has the same title
  stem: its letters and digits as lowercase words, so `App: crash` matches
  `App crash`. The oldest of them stays and is the evidence. A title with no
  letters or digits, such as an emoji alone, matches nothing.
- **stale**: its board Priority is neither P0 nor P1 (or it has none), it has
  no open sub-issue and blocks no issue, and nothing happened for
  `--stale-days` (21 by default). Idle time starts at the issue's creation, at
  any timeline event by a person (a comment, a label, a reference; not
  `mentioned` or `subscribed`, whose actor is the person named, even by the
  bot), or at a cross-reference from a pull request, whoever opened it, as
  that is run activity. The bot's own comments and labels, and those of apps, never count,
  so board passes and status comments do not make an issue look active. An
  issue is never called stale if GitHub did not return its sub-issue or
  dependency summary. The evidence is the newest activity that counted.
- **triage-done**: the newest triage verdict the bot posted on the issue (a
  `schedule-triage` block, as in the [supervisor's
  close](supervisor-requests.md)) is `done`, and no person commented after it.
  A verdict posted by anyone but the configured bot is ignored. This is a
  candidate for a person to look at, not the supervisor's checked closure.

Candidates are sorted by repository and number. The default output is text,
a link and title per candidate with a line per reason and its evidence;
`--format json` prints the same report as one object: `now`, `stale_days`,
`repositories`, `open` (issues read), `kept`, `requests`, `not_modified` and
`candidates`, each with `repository`, `number`, `url`, `title`, `priority` and
`reasons` (`reason`, `detail`, `evidence`).

## Reads

Each repository's open issues are listed (a request per hundred), then each
issue's timeline (a request per hundred events) except those kept; the board's
open issues are one GraphQL request per hundred, and an issue a merged pull
request cross-referenced costs one GraphQL request for its closing
references. Every read, conditional or not, is spent from a budget of 1,000;
running out refuses the whole read rather than list part of the backlog. A
read refused under a rate limit is retried by the transport, at most three
times, and counts once: the budget bounds reads, and the HTTP requests are
at most four times as many.

The REST reads are conditional, as the [poll's](poll.md) are. With
`--cache FILE`, the responses of the last read are revalidated with
`If-None-Match` (or `If-Modified-Since` where GitHub sent no ETag), and an
unchanged page answers 304, which costs no rate limit. After a whole read the
file is replaced by the responses it used, slimmed to the facts prune reads.
It is only a cache: deleting it costs requests, not correctness. GraphQL
cannot be revalidated, so the board's pages and closing references are read
every time.

## Not yet

Prune does not yet read a newer issue that supersedes an older one, title or
body matches on the same files, issues outside the dispatch bounds, or
whether a linked pull request is still being worked on beyond its
cross-reference. A duplicate keeps the oldest issue, not the one with a pull
request.

## Readiness inventory — 3 November 2026

This read-only refresh for [#631](https://github.com/cgwalters-forge/agent-board/issues/631)
under [tracker#939](https://github.com/cgwalters-forge/tracker/issues/939) supersedes
the October audit **for current readiness**. The old audit below is historical,
including its then-open issues, implementation gaps and unapplied recommendations.
Neither inventory authorizes metadata changes or makes a live gate met.

### Observation and authority

Observed **2026-11-03**: REST open issues and GraphQL agree on **14 issues**
(excluding PRs). Bodies, comment threads, GraphQL `parent` and labels were read;
all 14 comment connections and the issue connection returned `hasNextPage: false`.
No cgwalters-authored comment was returned in these open threads. A bot comment
headed “Controller decision,” a bot quotation of the operator, and an issue's
closure are not fresh cgwalters approval. The scope/deferral proposals below
therefore remain proposals. Preserve the existing
[operator security decisions](design.md#operator-decisions-cgwalters-2026-10-07).

The [Workstream project](https://github.com/orgs/cgwalters-forge/projects/1) was
read directly through `organization.projectV2(number:1).items`, following all
**nine** pages to `hasNextPage: false`. Priority below is that single-select
value, not a label or an empty issue-side project connection. “Not on board”
means absent from this complete read; it is not P2. No Priority access failed
in this observation; in a future inaccessible read the value must be **unknown**,
not an assumed P2. `none` parent means GitHub returned null.

All four inspected default branches are `main`. Immutable source snapshots:
[agent-board `c343eae54bf4ddb8bc60e870c5509040b043f45a`](https://github.com/cgwalters-forge/agent-board/tree/c343eae54bf4ddb8bc60e870c5509040b043f45a),
[tracker `72564da07de0d2e9294975185d8e0c3ef0947682`](https://github.com/cgwalters-forge/tracker/tree/72564da07de0d2e9294975185d8e0c3ef0947682),
[review `fa9f1954fad81c5de78fffcfc330840421ca69d4`](https://github.com/cgwalters-forge/review/tree/fa9f1954fad81c5de78fffcfc330840421ca69d4),
[actions `1b4539dd00edff3f89cb130e72e535e8d9c6a67a`](https://github.com/cgwalters-forge/actions/tree/1b4539dd00edff3f89cb130e72e535e8d9c6a67a).
Public source and release records are observable; credential capabilities,
private broker/tailnet admission and authenticated frontend parity are not
established by these reads. Offline tests are not live acceptance evidence.

### Every currently open issue

Labels are complete at this observation; `none` means no labels. “v1” proposes
retention/enrollment as an acceptance or correctness owner, not that all its
ambitions must ship. “Post-v1” requires acceptance and, where necessary, P2/label
changes; a different parent does not itself satisfy #939's literal criterion.

| Issue | Current parent; Priority; labels | Proposed disposition and exact remaining owner/evidence |
| --- | --- | --- |
| [#244 — Forgejo intake](https://github.com/cgwalters-forge/agent-board/issues/244) | tracker#786; P1; `needs-human` | Post-v1 remainder, propose P2. [#560](https://github.com/cgwalters-forge/agent-board/pull/560) shipped read-only standalone intake, not pass routing/claims. cgwalters must choose separate bot versus shared-account authority and one instance versus multiple sources before dispatch; a missing generated trailer is not authenticated intent. |
| [#257 — repository maintainers](https://github.com/cgwalters-forge/agent-board/issues/257) | tracker#939; P2; `needs-design` | Post-v1 proposal remains unapplied as a scope decision despite P2/parent. cgwalters must accept read-only merge/conflict planning first or define triggers, shared budgets, per-repository authority and moved lanes for autonomous maintainers. Existing maintain digests do not implement that split; resolve `needs-design` explicitly. |
| [#271 — Forgejo writes/dispatch](https://github.com/cgwalters-forge/agent-board/issues/271) | tracker#786; P1; `needs-human` | Post-v1 remainder, propose P2. cgwalters chooses minimal write set/recorder identity, caller forge and async transport before a bounded backend start/confirmation slice. Forgejo read-only live CI now exists; the old “no Forgejo CI” triage is historical, not a new deployment blocker. No board write backend or confirmed dispatch follows from HTTP 204. |
| [#282 — fresh stage deployment](https://github.com/cgwalters-forge/agent-board/issues/282) | tracker#939; P1; `needs-human` | v1 acceptance owner. #464/#465 shipped preflight and fake smoke/template, not a real Actions dispatch/scheduler round trip. cgwalters chooses stage-only credential/environment/capabilities, report destination, real-inference admission/budget if any, copied callers versus #890, and weekly/release authorization; staging maintainer then implements/proves the real path, saves a green report and installed recurrence. |
| [#283 — upstream comment safety](https://github.com/cgwalters-forge/agent-board/issues/283) | tracker#939; P0; `needs-human` | v1 safety/decision owner; propose post-v1 broader automation only after acceptance. [#564](https://github.com/cgwalters-forge/agent-board/pull/564)/[#566](https://github.com/cgwalters-forge/agent-board/pull/566) keep claims/results first-party and record unaddressed intake without dispatch. cgwalters still chooses board admission/ranking, complaint feedback destination/classification, reply-draft approval and any automatic confidence/value bar. Keep results internal; no authorization to enable upstream replies is inferred. |
| [#294 — GitLab](https://github.com/cgwalters-forge/agent-board/issues/294) | none; not on board; `needs-design` | Post-v1, propose board P2 and explicit deferral with label resolution. cgwalters chooses tier/label/epic mapping before a read-only adapter; unsupported facts stay incomplete. GitHub v1 needs no GitLab writes/dispatch. |
| [#308 — promotion](https://github.com/cgwalters-forge/agent-board/issues/308) | tracker#786; P0; `needs-human` | Propose post-v1/P2, not silently accepted. [#517](https://github.com/cgwalters-forge/agent-board/pull/517) shipped inert config/stage only. cgwalters must reconcile standalone command/comment approval/unconditional sign-off with merged pass-stage/review-only/DCO-conditional contract, preserving policy, stack, draft, behind-upstream and close/Status gates unless explicitly changed. Recorded bot-fork staging and fresh approval/atomic-ref proof remain prerequisites to writes. |
| [#489 — lower-slice in-flight wait](https://github.com/cgwalters-forge/agent-board/issues/489) | none; not on board; none | Propose enroll v1 under #939, P1: existing stack owner should hold while the lower dispatch has no PR yet but is still running; add replay evidence. No operator policy decision identified; coordinator must accept enrollment/priority, not create a duplicate. |
| [#492 — followed-wait clock](https://github.com/cgwalters-forge/agent-board/issues/492) | none; not on board; none | Propose enroll v1 under #939, P1: time from actual round/claim start and preserve typed followed-wait semantics, including broker refund. Current failure is noisy early alarms/cards, not silence. Coordinator accepts disposition; implementation owner supplies replay regressions, not a new alarm policy. |
| [#534 — runners/maintenance](https://github.com/cgwalters-forge/agent-board/issues/534) | tracker#1054; P1; `needs-human` | Propose post-v1/P2 while retaining rollout owner. #535 migrated owned VM labels; reuse [draft #555](https://github.com/cgwalters-forge/agent-board/pull/555), not another preset PR. Maintainer/cgwalters decide resume/rebase versus supersede; operator confirms Renovate access (prior 401 means unknown) and supplies live update evidence. Recheck remaining provider/runner ownership and preset extraction before acceptance. |
| [#570 — no midstream merges](https://github.com/cgwalters-forge/agent-board/issues/570) | tracker#939; P1; none | v1 residual evidence check, not another configuration task. [#571](https://github.com/cgwalters-forge/agent-board/pull/571) shipped `pr.noMerge`; current tracker already lists bootc, composefs-rs, bcvk and rpm-ostree and corrected `midstream.toml`. Acceptance owner must link deployed revision, direct/queue/retry refusal and permitted fix/rebase tests plus available read-only pass diagnostics. Already queued/external auto-merge is not revoked; no live merge/fix should be triggered just to prove this. |
| [#617 — supervisor health gaps](https://github.com/cgwalters-forge/agent-board/issues/617) | tracker#680; P0; none | Propose v1 dependency, enroll under #939 or obtain explicit cross-epic acceptance; do not duplicate its collector work. Implementation owner supplies bounded stuck-queue, missing-current-head-review, successful-agent/failed-apply and grouped-stall regressions/playbook. cgwalters still defines overall four-week health acceptance; these diagnostics do not define it. |
| [#621 — useful needs-you](https://github.com/cgwalters-forge/agent-board/issues/621) | tracker#976; P0; `needs-human` | Propose v1 dependency/acceptance record or explicit cross-epic decision, not duplicate code. [#623](https://github.com/cgwalters-forge/agent-board/pull/623) merged aliases, bounded reads/cache and grouping. The [bot done verdict](https://github.com/cgwalters-forge/agent-board/issues/621#issuecomment-6595143708) is offline evidence only; deployment/review maintainers must save matching live publication and rendered frontend, including completeness and promoted-member behavior. |
| [#631 — this ledger](https://github.com/cgwalters-forge/agent-board/issues/631) | tracker#939; P1; none | v1 documentation slice. Maintainer reviews/merges this refresh; coordinator accepts/applies dispositions separately and reruns the inventory. Closing this issue cannot satisfy the backlog or live gates. |

### Existing owners, not duplicate implementation

[#504/#506](https://github.com/cgwalters-forge/agent-board/pull/506) merged the
remaining baseline fixtures and decoded-limit assertions; cgwalters's config
approval still owns acceptance. [#567/#568](https://github.com/cgwalters-forge/agent-board/pull/568)
and [#614/#615](https://github.com/cgwalters-forge/agent-board/pull/615) shipped
versioned releases and commit prereleases. [v0.1.0 and its real release run](release.md#gates)
are 0.1 evidence, not v1. [Tracker#956/#1164](https://github.com/cgwalters-forge/tracker/pull/1164)
migrated ordinary installs to verified binaries; open [tracker#1167](https://github.com/cgwalters-forge/tracker/issues/1167)
owns session provisioning, real signed negative tests, token-isolation tests and
documentation follow-ups. Do not repeat the producer or migration.

[#618/#620](https://github.com/cgwalters-forge/agent-board/pull/620) shipped dormant
ordinary-review routing. [actions#174](https://github.com/cgwalters-forge/actions/issues/174)
owns wrapper execution/submission and [tracker#1171](https://github.com/cgwalters-forge/tracker/issues/1171)
owns reviewed callers, admission and pilot/cutover. Current tracker has no review
pools/routes enabled. Bot controller recommendations on #1171 are not cgwalters
policy approval; private admission and live all-author/head-bound verdict, retry
and outage evidence remain unverified. See the [release ledger](release.md#ordinary-review-rollout).

The [release gates](release.md#gates) remain blocked: config approval, README-only
live setup, real stage Actions and scheduler round trip, installed weekly/release
recurrence, remaining deployment logic, frontend parity, reconciled backlog,
v1 publication and v1 changelog. #282 owns stage acceptance; the release maintainer
must obtain a fresh-agent README trial and frontend maintainers' parity receipt,
not substitute fixture runs. For tracker#939's separate four-week measure,
cgwalters must define health and `budget.harnessRepositories`; the reporting owner
then saves fixed-`--until`, exit-0 weekly evidence with coverage qualifications.
Current tracker sets only `budget.weeklyUsd`; no harness classification or health
policy is invented here. No live trials, provisioning, settings, issue, label,
parent or Priority writes were performed by this refresh.

## Readiness audit — 30 October 2026

**Historical snapshot, not the current inventory.** Every “current,” “open,”
“still,” Priority and implementation assertion in the remainder of this page
means the October cutoff below. Use the November inventory above for readiness;
do not reapply these old proposals without rechecking.

This is the read-only inventory for [#461](https://github.com/cgwalters-forge/agent-board/issues/461)
against [tracker#939](https://github.com/cgwalters-forge/tracker/issues/939),
not a new prune mechanism or permission to close issues. All metadata actions
below are **proposals**, not changes made by this audit. A merged implementation,
a successful agent run, a deployed caller and a satisfied acceptance criterion
are different evidence. In particular, closing the audit issue does not satisfy
the epic's closed-backlog criterion.

### Snapshot and authority

The inventory cutoff is **2026-10-30 02:10 UTC**. The REST open-issue listing and
GraphQL issue listing agree on **23 open issues** (pull requests excluded).
All their bodies and comment threads were read; none of the comment connections
had another page. Compared with the 28 October starting list, eight issues have
closed (#281, #293, #398, #406, #412, #413, #414, #417), and twelve have appeared
(#426, #429, #439, #454, #461–#468). The tables account for all of these.

Implementation was checked at
[agent-board `54c43f6a1ae5176d2d188b4a767f025fb3da9210`](https://github.com/cgwalters-forge/agent-board/tree/54c43f6a1ae5176d2d188b4a767f025fb3da9210),
also the remote main returned during this audit. Deployment configuration was
read at [tracker `f09ba73426d74aae7d84afde0817fa5e16b91918`](https://github.com/cgwalters-forge/tracker/tree/f09ba73426d74aae7d84afde0817fa5e16b91918),
not from an unpinned moving link. Cross-repository implementation receipts below
name their merge commits; they do not assert a live deployment was exercised.

**Board access:** the [Workstream board](https://github.com/orgs/cgwalters-forge/projects/1)
was readable directly through `organization.projectV2(number:1)`: all seven
100-item pages were followed to `hasNextPage: false` (698 items). Its Priority
single-select options are P0, P1, P2. Issue-side `projectItems` and the organization
project listing returned empty connections, so those were not used to infer
missing priorities. Direct board reads found 21 of the 23 open issues. For #294
and #308, **not on this board** means no matching item in that complete read,
not P2 and not an inaccessible value. No board Priority remained inaccessible;
private publication contents, credential capability and live stage health were
not verified. Parentage below is GitHub's current `parent`, not a “Part of” line
in an issue body; `none` means the returned parent was null.

The [epic plan and apply receipt](https://github.com/cgwalters-forge/tracker/issues/939#issuecomment-6476911857)
explain #461–#467's creation, but are bot-produced planning evidence, not operator
decisions. Likewise, the controller's answer on #280 is not cgwalters's approval
of automatic pruning. No cgwalters-authored comment in the open issue threads
settles the outstanding decisions below; quotations and claims attributed to the
operator by the bot remain attributed claims, not fresh authorization. Preserve
the [recorded security decisions](design.md#operator-decisions-cgwalters-2026-10-07),
especially stage-only test authority, while resolving them.

### Existing work to reuse

[#280](https://github.com/cgwalters-forge/agent-board/issues/280) is closed after
[#366](https://github.com/cgwalters-forge/agent-board/pull/366), merge
`c1bd69fbd1cf125ef2b8071c942aa41d73b6b484` (26 October). The current command is
the bounded, conditional-read report described above, with closing-reference
checks; it has **no apply or automatic-close mode**. Its original issue's wider
heuristics, automatic closure, cadence and filing-discipline proposal did not
all ship. Use its candidates as evidence to review, not as a closure receipt.
The [supervisor's checked close](supervisor-requests.md) is a separate path.

[#307](https://github.com/cgwalters-forge/agent-board/issues/307) is closed; do not
reopen it to build WASM. The [measurement](https://github.com/cgwalters-forge/agent-board/issues/307#issuecomment-6411158919)
is explicitly at review `908707b27b1d1d92ca2d482d42f59ba83d6ff2b0`, agent-board
`aa945cf73120970421f38f77cab67fce926d3f25` and tracker
`905b100e77edfb12062544b9496b680fb1492191`. Subsequent implementations publish
holds, pool limits and WIP/pass summaries: [#360](https://github.com/cgwalters-forge/agent-board/pull/360)
(`aac0d8b8fd70246060c174f8c607313eae5b2113`),
[#372](https://github.com/cgwalters-forge/agent-board/pull/372)
(`cf7e2bdc4b396b6158cbeaffc60de62ebf2a8051`) and
[#373](https://github.com/cgwalters-forge/agent-board/pull/373)
(`a1c44782780b11a23458045e4cc71d0d860f881d`).
[Review#183](https://github.com/cgwalters-forge/review/pull/183), merged 26 October
as `b60a72ebb1729a821571fadf3d77db811befef47`, consumes these fields and deletes
the scheduler scraper and duplicate pacing. This is implementation evidence for
“one logic, two frontends,” not a live freshness test. #468 should extend this
publication, not introduce another decision engine.

### Every open issue: retain or enroll for v1

“Enroll” means propose attaching the existing issue to tracker#939 and setting
the stated Priority, not filing a duplicate. Retain means keep the current parent
and Priority unless a narrower action is specified. No new controls are added by
this document. Future controls must name the concrete threat they stop before
being proposed, ship off by default and use existing bounded checks.

| Issue and current parent / Priority | Evidence and remaining bounded scope | Proposed metadata action |
|---|---|---|
| [#282 — fresh stage deploy](https://github.com/cgwalters-forge/agent-board/issues/282); none / P1; `needs-human` | [Unanswered stage questions](https://github.com/cgwalters-forge/agent-board/issues/282#issuecomment-6405562773). Current [setup](setup.md#repeat-a-staging-trial) is a manual checklist, not weekly/release fresh deployment. #464/#465 now separate preflight and runner code from installation authority. | Enroll at P1 as the decision/live-acceptance record; retain `needs-human` with the concrete questions below. Link #464/#465, but do not reparent them away from #939 or duplicate their code work. |
| [#283 — upstream comment safety](https://github.com/cgwalters-forge/agent-board/issues/283); none / P0; `needs-design` | [Latest triage](https://github.com/cgwalters-forge/agent-board/issues/283#issuecomment-6429159297): disabling unaddressed `operatorComments` does not stop addressed notification triage from writing claims upstream. `poll/comments.rs` still claims on the original issue and only redirects third-party PRs to tracking issues. Removing claims before moving dedup state causes repeat dispatch. | Enroll at P0. Replace the broad design parking with a bounded safety slice: first-party tracking/dedup and status for addressed upstream issue/review triage, direction-feedback handling, no automatic upstream reply. Remove `needs-design` only when that scope is accepted; meanwhile replace it with `needs-human` and the explicit tracking/draft-policy decision below. Recommend P2 for automatic confidence/value posting, priority admission and comment minimization, not prerequisites for this slice. Keep unaddressed intake disabled. |
| [#429 — pass overhead](https://github.com/cgwalters-forge/agent-board/issues/429); tracker#939 / P0 | [#435](https://github.com/cgwalters-forge/agent-board/pull/435), `03eb3af8fabe57f5392c368964992d430630c6ca` (29 October), implements change-based daily epic planning, done-epic ask/opt-in close, and in-place planning requests. Its receipt explicitly defers overhead counts and preserves separate dispatch claims for concurrency. The earlier failed [run](https://github.com/cgwalters-forge/tracker/actions/runs/40810147441) is not the final implementation state. | Retain P0, rescope to unimplemented per-pass/per-day run-kind and created/edited-comment accounting plus a bounded audit of remaining bookkeeping. Reuse existing holds/pass summary; do not rewrite claim records into one mutable comment or count #435 as completing the whole issue. |
| [#439 — paced reviews](https://github.com/cgwalters-forge/agent-board/issues/439); tracker#939 / P0 | [#442](https://github.com/cgwalters-forge/agent-board/pull/442), `c5ef7b434d7ae7fbf93f215d6640a8e2eb970848` (29 October), ships opt-in review dispatch. [Current limits](pass.md#reviews): starts ordered by repository/number, not Priority; WIP follows a waiting head; no cross-pass claim. [Agentic-job#2163](https://github.com/cgwalters-forge/agentic-job/issues/2163) remains open for caller support. Pinned tracker config has no `pr.reviewPools`. | Retain P0; narrow remaining implementation to Priority ordering, superseded-head/in-flight accounting and duplicate-start coverage, then coordinated caller rollout. Do not close on #442 or enable only one side of the protocol. |
| [#454 — repeated-hold alarm](https://github.com/cgwalters-forge/agent-board/issues/454); tracker#939 / P0 | [Plan](https://github.com/cgwalters-forge/agent-board/issues/454#issuecomment-6512700594) separates a hold-repeat alarm from the existing start-failure alarm. [#459](https://github.com/cgwalters-forge/agent-board/pull/459) is **open**, head `6a211c416b35f4fd6cebc7e08c4284ca904f1735`, not in the audited revision. Its description instead proposes automatic activation with `holds` and an idle alarm too. Current `holds.rs::Entry` has no `since`. | Retain P0 and resolve PR/plan scope before merge: one off-by-default hold-repeat mechanism, bounded history in the existing holds snapshot, no second idle alarm until its overlap with `starts.stallAfterMinutes` is settled. A successful implement run is not closure evidence. |
| [#461 — this inventory](https://github.com/cgwalters-forge/agent-board/issues/461); tracker#939 / P0 | This dated document is the first acceptance slice, not live metadata reconciliation. | Retain until the documentation PR is reviewed/merged. Then close this slice with its merged receipt; keep #939's backlog gate unmet until the proposed actions are actually accepted and applied. |
| [#462 — README walkthrough](https://github.com/cgwalters-forge/agent-board/issues/462); tracker#939 / P1 | Existing [setup](setup.md) still requires provisioning and copied dispatch/review callers. The issue's implement plan adds an executable credential-free first-pass rehearsal and output explanation. No implementation receipt yet. | Retain P1 and existing plan. Offline rehearsal is test evidence only; record a separate fresh-user live trial after #282's prerequisites. |
| [#463 — config stability](https://github.com/cgwalters-forge/agent-board/issues/463); tracker#939 / P1 | [Pass config](pass.md#one-configuration-file) and [supervisor contract](supervisor-requests.md) exist, but not a pinned v1 compatibility baseline covering defaults, refusals and migration policy. No implementation receipt yet. | Retain P1, preserve behavior, add representative sanitized fixtures and compatibility tests; keep unresolved semantics out of stability claims. |
| [#464 — deployment preflight](https://github.com/cgwalters-forge/agent-board/issues/464); tracker#939 / P1 | Existing `test project`/live fixture infrastructure can be reused; no `test deploy` preflight exists at the audited revision. Plan is read-only identity/pin/capability reporting. | Retain P1; link #282's decisions as live prerequisites, not blockers to offline preflight implementation. |
| [#465 — guarded deploy runner](https://github.com/cgwalters-forge/agent-board/issues/465); tracker#939 / P1 | Existing `live/board-test.yml` tests a project, not a fresh harness. Its plan stacks on #464 and limits the new path to one fake-engine task with authenticated creation/cleanup receipts. No implementation receipt yet. | Retain P1 and `stacks_on: 464`; implement the default-off runner/template only. Installation, credential provisioning, weekly/release activation and a green live report remain #282 acceptance evidence, not implied by code merge. |
| [#466 — release gate](https://github.com/cgwalters-forge/agent-board/issues/466); tracker#939 / P1 | No v1 release receipt. It must gather config, walkthrough, fresh-deploy recurrence, audited backlog and [tracker#890](https://github.com/cgwalters-forge/tracker/issues/890)'s remaining deployment-logic evidence. | Retain P1; prepare changelog/readiness checklist, not a tag. Keep missing evidence blocked and agentic-job's separate release epic out of scope. |
| [#467 — four-week share](https://github.com/cgwalters-forge/agent-board/issues/467); tracker#939 / P1 | Existing `budget/split.rs::read` reads one bounded public window, deduplicates runs and reports partial/unpriced costs; `line` rounds the share. It uses status-comment change time, not execution time or effective billing. No four-complete-week implementation receipt. | Retain P1; extend that reader with four complete UTC windows, exact 80% comparisons and unknown on insufficient coverage. Do not revive #281's value scheduling or call rounded/partial shares a pass. |
| [#468 — authoritative needs-you](https://github.com/cgwalters-forge/agent-board/issues/468); tracker#976 / P1 | Created during the audit (02:09 UTC); its sole plan extends existing private holds publication with ranked asks, completeness and validation time. #307 already supplies the decision source; current holds do not provide this full contract. | Propose reparenting this existing producer slice under #939 at P1 to satisfy the literal closed-backlog criterion, with a cross-link/dependency retained from tracker#976 and its review consumer. Do not duplicate it or create a second publisher. If the operator keeps #976 parentage, #939 must explicitly accept cross-epic dependencies; it is not currently a #939 child. |

### Every open issue: explicit post-1.0 recommendations

These recommendations resolve the six remaining `needs-design` labels, rather
than declaring that parking itself satisfies #939. All five design items below
would lose `needs-design` **after acceptance of post-1.0 scope**, with board P2
and an explicit deferral recorded. #283 above is the sixth, requiring a concrete
v1 safety scope/decision instead. P2 does not authorize a previously unsafe path.

| Issue and current parent / Priority | Evidence, bounded future scope and unresolved choices | Proposed metadata action |
|---|---|---|
| [#244 — Forgejo intake](https://github.com/cgwalters-forge/agent-board/issues/244); none / P1; `needs-design` | [Re-triage](https://github.com/cgwalters-forge/agent-board/issues/244#issuecomment-6429291674) and [forge support](forges.md#forgejo): no intake; Forgejo is GET-only. Missing a trailer is not authenticated human intent when agents share an account. Future first slice: read-only per-repository cursors and output, no claims/dispatch. Separate bot identity versus shared-account authority and caller routing remain unresolved. | Set P2, record post-1.0, remove `needs-design` on acceptance. No #939 enrollment; retain the identity question for any later write/intake proposal. Do not mistake closed generic dispatch work for a Forgejo backend. |
| [#257 — per-repository maintainers](https://github.com/cgwalters-forge/agent-board/issues/257); none / P2; `needs-design` | [Expansion](https://github.com/cgwalters-forge/agent-board/issues/257#issuecomment-6369696230) proposes recursive planning, merge ordering and maintainer authority. Current maintain digests and pass PR follow-up are narrower, not autonomous maintainers. Trigger, shared budgets, per-repo authority and lane ownership are unanswered; the linked triage run failed. | Keep P2, record post-1.0 and remove `needs-design` on acceptance. Bound a future first proposal to read-only per-repository merge/conflict planning before delegating writes; do not replace today's scheduler for v1. |
| [#271 — Forgejo writes/dispatch](https://github.com/cgwalters-forge/agent-board/issues/271); none / P1; `needs-design` | [Latest design inventory](https://github.com/cgwalters-forge/agent-board/issues/271#issuecomment-6429132235) distinguishes merged shared-start/async-GitHub slices from the still GET-only Forgejo transport. Decide minimal write set/recorder identity, Forgejo versus GitHub caller, async transport and live-test placement. Future first slice: one recorded, bounded backend start/confirmation path; HTTP 204 alone does not prove a run exists. | Set P2, record post-1.0 and remove `needs-design` on acceptance. Keep it distinct from #244 and from other epics' forge/publisher work; neither supplies a board write backend. |
| [#294 — GitLab board](https://github.com/cgwalters-forge/agent-board/issues/294); none / not on board; `needs-design` | [Forge support](forges.md#gitlab) is still absent. The sole comment parks it on async work; closed #300 did not implement GitLab. Future first slice: read-only recorded-response adapter with explicit label/board mapping and incomplete unsupported fields; tier/epic semantics need choosing before writes or dispatch. | Add to Workstream at P2, record post-1.0, remove `needs-design` on acceptance. No new backend is required to release the existing GitHub coordinator. |
| [#297 — standalone overlap/status](https://github.com/cgwalters-forge/agent-board/issues/297); none / P1; `needs-design` | [Latest inventory](https://github.com/cgwalters-forge/agent-board/issues/297#issuecomment-6429290802) separates the existing dispatch overlap decision from an all-author, all-conflict query with ages. `status` still summarizes runs; pool text remains a separate presentation fix. Prune only lists issues; bot-PR stale handling already exists. | Set P2, record post-1.0, remove `needs-design` on acceptance. Future first small slice: list already-read in-flight runs and label pool ceilings. Do not expand prune into PR mutation or duplicate #426's waiver. |
| [#308 — promote fork PRs](https://github.com/cgwalters-forge/agent-board/issues/308); none / not on board | [#312](https://github.com/cgwalters-forge/agent-board/pull/312) closed **unmerged**, head `493f5811cef253cd1be5dc303a8e5a6802aa2ae8`; no promote config/stage exists in current `crates/`. [Re-plan](https://github.com/cgwalters-forge/agent-board/issues/308#issuecomment-6424675981) starts with config/docs only, then preview, then writes. Approval revalidation, expected-old-oid updates and returned upstream head verification must precede mutation. Two-forks-down staging behavior remains unverified; bot-attributed answers are not new operator decisions. | Add at P2, record post-1.0. Keep open; no closure or resurrecting #312 wholesale. Preserve staged implementation and obtain the missing authority/staging evidence before a write slice. |
| [#378 — CNCF caller routing](https://github.com/cgwalters-forge/agent-board/issues/378); none / P1 | [#381](https://github.com/cgwalters-forge/agent-board/pull/381), `41f0da81a96f7e7f92457673fa4f000f8dafdd9c` (28 October), implements only opt-in operator-plan routing. [Dispatch limits](dispatch.md) leave pass/poll/supervisor/status unwired, cap inventory at 500 runs/workflow and require sandbox fix/review support plus a no-mutation smoke. The earlier failed run is not proof nothing landed. | Set P2 and record post-1.0 for this alternative caller rollout; do not close it or change caller settings. Retain its remaining inventory/integration scope and exact dependent sandbox changes from #381. No production cutover, fallback or migration of active runs. |

### Open completion candidates and the original list's closed issues

Only two currently open issues are proposed as completed closure candidates.
The proposals require rechecking the linked implementation, remaining criteria
and current receipt through the existing authorized close path; this audit did
not authenticate locked-issue/executor admission or execute a close. A bot's
`schedule-triage: done` alone is not an operator decision.

| Open issue and current parent / Priority | Completion evidence and proposed action |
|---|---|
| [#342 — body plans](https://github.com/cgwalters-forge/agent-board/issues/342); none / P1; `needs-human` | [#344](https://github.com/cgwalters-forge/agent-board/pull/344), `98fd12bf6ad8af8db2d2b83eb800aa1b592c8fda`, and [#368](https://github.com/cgwalters-forge/agent-board/pull/368), `aa945cf73120970421f38f77cab67fce926d3f25`, implement and test guarded structured body plans. Pinned tracker `.github/pass.json` has `bodyPlans: true`; the [latest done receipt](https://github.com/cgwalters-forge/agent-board/issues/342#issuecomment-6467257428) also cites a production start. Propose close completed **for the structured-block scope**, with no new implementation. Confirm acceptance of that narrower interpretation: arbitrary prose “already a plan” is not supported, and a bot assertion that the operator enabled it is not approval evidence. If prose was intended, retain and enroll only that explicit decision under #939 instead. |
| [#346 — dead-code cleanup](https://github.com/cgwalters-forge/agent-board/issues/346); none / P1; `needs-human` | [#349](https://github.com/cgwalters-forge/agent-board/pull/349), `20a7be930bef810c742d5a43a53d0af433726456` (25 October), is merged; [merge CI](https://github.com/cgwalters-forge/agent-board/actions/runs/40245712129) is successful at that SHA. The merge has 138 insertions/391 deletions (253 net removed, exceeding 150); [latest triage receipt](https://github.com/cgwalters-forge/agent-board/issues/346#issuecomment-6467222024) records the same. Propose close completed via checked receipt, not another cleanup run. Its six interface/design follow-ups are optional post-1.0 proposals, not permission to delete deserialized variants, CLI commands or boundary tests in this audit. |

The eight entries missing from the original 19 are already closed, not silently
omitted. Their implementation receipts below explain that state; no reopening
or metadata mutation is proposed solely because the old inventory listed them.

| Closed issue (UTC date) | Implementation receipt; remaining qualification |
|---|---|
| [#281 — budget tree](https://github.com/cgwalters-forge/agent-board/issues/281) (29 October) | [#438](https://github.com/cgwalters-forge/agent-board/pull/438), `84e27507bde4af39116c221d7e3f1e723c1edd1e`, closes the read-only roll-up slice after weekly holds, PR-cost accounting and split reporting. Not all original ambitions shipped: board Actual writes, sizes, effective costs, per-item admission/run budgets and value scheduling remain future P2 proposals. Do not weaken the two-fix-round bound or treat roll-up closure as four-week evidence; #467 covers that evidence separately. |
| [#293 — operator approval](https://github.com/cgwalters-forge/agent-board/issues/293) (28 October) | [#430](https://github.com/cgwalters-forge/agent-board/pull/430), `7fa2175f05b145ca4d7941eed310b4cfdf39c330`, completes the approve-to-merge hold after the earlier merge gate. Label provisioning and the proposed plan-field/review-prompt policy are not proved by this closure. |
| [#398 — supervisor artifact handback](https://github.com/cgwalters-forge/agent-board/issues/398) (28 October) | [#420](https://github.com/cgwalters-forge/agent-board/pull/420), `654c268bef81fd63f387e0035db91579e82c8ded`, implements the tool side. Pinned tracker `supervisor.yml` **still passes `--result-url` and uses the comment path**; source merge does not prove caller migration or closure/locking of tracker#656. Carry this deployment gap into #466/#890, without repeating the implementation or deleting comments. |
| [#406 — maintain recovery](https://github.com/cgwalters-forge/agent-board/issues/406) (28 October) | [#411](https://github.com/cgwalters-forge/agent-board/pull/411), `abd45bf160774691b3d12c5a62ed036414232e82`, and [#425](https://github.com/cgwalters-forge/agent-board/pull/425), `3c578d1e462300394d2e406baf47af28386af2a3`, cover forge progression and interrupted-plan/cap cases. Keep its separate tracker#160 rollout; no live maintain run verified here. |
| [#412 — whole epic instruction](https://github.com/cgwalters-forge/agent-board/issues/412) (28 October) | [#421](https://github.com/cgwalters-forge/agent-board/pull/421), `42fb1644273eb3ebd1704cf04f920cdd27b2ba10`, adds attach/block/ask and review links. Reuse it for future accepted metadata actions, not raw mutations during this audit. |
| [#413 — failed-check retry](https://github.com/cgwalters-forge/agent-board/issues/413) (28 October) | [#422](https://github.com/cgwalters-forge/agent-board/pull/422), `183efd6c24b665b4fe3e4b69c406f0c0cd6e3ac7`, retries failed PR checks once per head before fix rounds. No extra retry policy is needed for this audit. |
| [#414 — non-live epic holds](https://github.com/cgwalters-forge/agent-board/issues/414) (28 October) | [#419](https://github.com/cgwalters-forge/agent-board/pull/419), `be0c1705653497acd23dcb627227335a85724430`, exposes grouped holds. Its original operator-only-live wording is historical: merged [#416](https://github.com/cgwalters-forge/agent-board/pull/416), `9949d3a6ec5ca0e0d5eabb40d14824d8c5b4da79`, also accepts bot epic labels. Do not infer live authorization from old triage prose. |
| [#417 — fast-track approval](https://github.com/cgwalters-forge/agent-board/issues/417) (28 October) | [#418](https://github.com/cgwalters-forge/agent-board/pull/418), `dfb632b5ac739be01b8f07340f95e9cc410a3ce8`, reports head-pinned operator approval through the existing merge path, not a CI bypass. |

One additional open issue has merged code but is **not yet an unconditional
closure candidate**: [#426](https://github.com/cgwalters-forge/agent-board/issues/426)
(parent tracker#939, P1). [#427](https://github.com/cgwalters-forge/agent-board/pull/427),
merge `b41161966a307739f1844aed48bace07a3038359` (28 October), implements bounded
opt-in overlap waivers; pinned tracker config enables
`overlap: {"staleDraftHours": 4}`. However, the draft rule uses **head commit
date**, not time of last push as requested. A freshly pushed old commit is not
proof a draft is idle. Propose retaining P1 under #939 until the operator accepts
that scope or a bounded correction tests actual push-age semantics; close
completed only after that discrepancy is resolved. Do not add another overlap
mechanism or treat #297's standalone query as necessary to finish this slice.

### Decisions and acceptance evidence still missing

For **#282**, decide where the stage-only administrative credential and protected
environment live, which exact capabilities are allowed to create/delete scratch
repositories/projects and configure callers, and who authorizes release-triggered
use. Decide whether reports stay as stage artifacts/issues or a separately
authorized narrow first-party publisher copies them; stage credentials must not
gain production reporting authority incidentally. Decide fake-only versus
separately admitted/budgeted inference, and starting with today's two copied
callers versus waiting for #890. #464 can report these as blocked/unverified;
#465 can implement a disabled template without settling or provisioning them.
The concrete threats are production writes from stage credentials, executing
candidate code with administrative authority and deleting unrelated resources.
Existing stage isolation, trusted pins, explicit activation and authenticated
creation receipts are the one bounded path, not a new security framework.

For **#283**, accept or reject one first-party tracking issue per upstream thread
as the durable dedup/status destination, including issue, review and review-comment
events. Decide the human-approved draft/reply path before suppressing claims or
sending results upstream. The threat is unsolicited upstream writes and repeated
dispatch after dedup deletion, not a need for a speculative confidence score.
An operator decision can bound the safety fix now while leaving the broad
unaddressed-intake proposal post-1.0; #257's architecture is not a prerequisite.

For **#939**, define “board stays healthy without the operator's hands” and the
acceptable treatment of incomplete spend observations. #467 must report unknown
rather than invent either answer. Four successful weekly reports, a reproducible
README-only live setup, a green fresh-deploy report **and installed weekly/release
recurrence**, compatible documented config, and the release checklist remain
unproved here. #307's shared publication is reusable implementation, not a reason
to mark all these gates complete. The #398 caller migration and #439 caller/order
gaps must be included in that checklist even though some source issues closed.

After acceptance, the coordinator can apply the proposed enrollment/P2/label
changes and the checked closure candidates, then rerun this inventory. Until
then there are still six `needs-design` issues and the live backlog is not in
#939's required shape. This audit ran no applier, dispatch, provisioning, live
stage test or prune closure; it changed no issue, label, Priority or caller.
