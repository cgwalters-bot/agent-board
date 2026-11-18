# Role-prompt evaluation: historical sample preparation

> This text is LLM generated.

## Selection rationale

On 2026-11-17, inspected the latest 20 closed agent-board PRs and chose the
two most recent merged `cgwalters-bot` small symptom fixes with completed,
non-partial implement handbacks: [merge overlap](https://github.com/cgwalters-forge/agent-board/pull/1036)
and [source punctuation](https://github.com/cgwalters-forge/agent-board/pull/1031).
Dependency pin updates and the earlier, broader scheduling changes were not
selected. Both are deterministic regressions with existing fake-transport or
string-processing harnesses, not live services. This convenience sample does not
establish generality across repositories or task types. Selection used recency,
scope and reproducibility, not comparison-arm outcomes. No arm ran.

Both tasks have retained task text, immutable starts, check pins and observed
fail/pass results in [manifest.json](manifest.json), and their historical sizing
fits every ceiling under the protocol's token definition below. The manifest stays
pending only until the configuration slice fills it; no new option, launcher or
caller setting is added.

## Historical sizing

[preflight.json](preflight.json) links the operator's decision and both standing
controller decisions, and names the repositories, harness, pool tiers and artifact
destination. All three were retrieved with `gh api`. They are not capacity
measurements.
The capacity-reservation gate below is historical; current admission is in
[the protocol](../docs/eval.md#admission).

Per-task evidence retains selected nonsecret fields from each dispatch's
`summary.json`, task provenance, commands and observed exit codes:

| Task | Input / output / cache-read tokens | Input + output / ceiling | Total with cache reads / ceiling | Seconds / ceiling | Metered requests / `turns` ceiling | Conversation turns | USD / ceiling |
| --- | --- | --- | --- | --- | --- | --- | --- |
| [merge-overlap](tasks/merge-overlap/evidence.json) | 156431 / 13257 / 1181115 | 169688 / 200000 | 1350803 / 200000 | 1496 / 1800 | 31 / 100 | unknown | 1.1271 / 5 |
| [source-period](tasks/source-period/evidence.json) | 118901 / 14932 / 1659260 | 133833 / 200000 | 1793093 / 200000 | 961 / 1800 | 35 / 100 | unknown | 1.1061 / 5 |

Two token rules apply to these numbers. Under the original reading, `tokens`
bounded the total including cache reads, and neither task fits. The
[2026-11-17 rescope](https://github.com/cgwalters-forge/agent-board/issues/1038#issuecomment-6963338151),
which also rewrote the task issue's sizing paragraph, redefined `tokens` as
input plus output after both candidates exceeded 200000 only once cache reads
were added; under that rule both tasks fit
([admission](../docs/eval.md#admission)). The `usd`, `wallSeconds` and `turns`
ceilings are unchanged. The reported total is input + output + cache reads;
exposed reasoning is not added again. Cache writes and conversation turns are
null in both summaries.

The requests column is request-cap sizing: Praxis's finished count of metered
requests, with zero unmetered, compared with the `turns` ceiling because a cell's
`max-requests` cap is set from it. It is not a conversation-turn count, which is
unknown for both tasks. Cache reads are no longer bounded by the token screen.
The `usd` screen is after the fact, uses a historical list price that is not a
future common cost basis, and the harness budget can overshoot, so a cache-heavy
cell can exceed these totals and still fit. The configuration slice must set each
cell's timeout and request cap and name the cached-input price used for `usd`.
Missing metrics are not zero.
An additional lead, [misctool's retry fix](https://github.com/cgwalters-forge/misctool/pull/16),
was inspected after finding this blocker; its [dispatch](https://github.com/cgwalters-forge/tracker/actions/runs/42017179255)
also reported 898167 total tokens, so it was not substituted to suggest a fit.

### Continuation sizing search (2026-11-17)

Before the token definition was settled, when totals still counted cache reads,
a continuation checked smaller agenthome changes before spending time building
replacement regression bundles. The search rule was to inspect merged bot PRs
with small, deterministic changes and a tracker dispatch, first recent standalone
license/guidance changes, then older short guidance and configuration changes.
The inspection order below records this bounded search, not an exhaustive search
of the permitted repositories. No comparison arm ran or influenced selection.
The larger recent installer, renderer, supervisor and evaluation changes were
not pursued as smaller replacements. Agenthome PRs
[42](https://github.com/cgwalters-forge/agenthome/pull/42) and
[43](https://github.com/cgwalters-forge/agenthome/pull/43) were also inspected, but
their bodies did not supply tracker dispatch links, so no sizing was attributed
to them.

Each artifact link below identifies `dispatch-implement-agent-run/summary.json`.
All eight summaries reported `praxis-run-usage/v2`, `state=finished` and
`unmetered=0`. Totals are the reported input + output + cache-read tokens;
separately exposed reasoning is not added twice. Every summary has null cache
writes and conversation turns. Requests are reported separately, not substituted
for conversation turns. Only nonsecret sizing fields are transcribed here.

| Historical PR | Dispatch summary artifact | Input / output / cache-read tokens | Total / 200000 | Seconds / 1800 | Requests / 100 |
| --- | --- | --- | --- | --- | --- |
| [agenthome 84: licenses](https://github.com/cgwalters-forge/agenthome/pull/84) | [43440407086 / 14045364448](https://github.com/cgwalters-forge/tracker/actions/runs/43440407086/artifacts/14045364448) | 44870 / 7069 / 415616 | 467555 / 200000 | 243 / 1800 | 25 / 100 |
| [agenthome 82: thin guidance](https://github.com/cgwalters-forge/agenthome/pull/82) | [43306792368 / 13984467543](https://github.com/cgwalters-forge/tracker/actions/runs/43306792368/artifacts/13984467543) | 62371 / 5512 / 590080 | 657963 / 200000 | 247 / 1800 | 28 / 100 |
| [agenthome 78: durable findings](https://github.com/cgwalters-forge/agenthome/pull/78) | [43023428453 / 13855584848](https://github.com/cgwalters-forge/tracker/actions/runs/43023428453/artifacts/13855584848) | 106424 / 7639 / 903808 | 1017871 / 200000 | 270 / 1800 | 35 / 100 |
| [agenthome 34: review delegation](https://github.com/cgwalters-forge/agenthome/pull/34) | [39677380475 / 12382802573](https://github.com/cgwalters-forge/tracker/actions/runs/39677380475/artifacts/12382802573) | 48125 / 7739 / 541755 | 597619 / 200000 | 79 / 1800 | 18 / 100 |
| [agenthome 35: forge text](https://github.com/cgwalters-forge/agenthome/pull/35) | [39677359699 / 12381889460](https://github.com/cgwalters-forge/tracker/actions/runs/39677359699/artifacts/12381889460) | 30149 / 6476 / 553660 | 590285 / 200000 | 83 / 1800 | 18 / 100 |
| [agenthome 37: conditional navigation](https://github.com/cgwalters-forge/agenthome/pull/37) | [39677373689 / 12381208607](https://github.com/cgwalters-forge/tracker/actions/runs/39677373689/artifacts/12381208607) | 62937 / 11320 / 723404 | 797661 / 200000 | 147 / 1800 | 20 / 100 |
| [agenthome 46: encrypted reasoning](https://github.com/cgwalters-forge/agenthome/pull/46) | [40916971678 / 12916837070](https://github.com/cgwalters-forge/tracker/actions/runs/40916971678/artifacts/12916837070) | 489928 / 3872 / 105088 | 598888 / 200000 | 114 / 1800 | 25 / 100 |
| [agenthome 48: model prices](https://github.com/cgwalters-forge/agenthome/pull/48) | [40949750338 / 12930568737](https://github.com/cgwalters-forge/tracker/actions/runs/40949750338/artifacts/12930568737) | 78423 / 7234 / 580219 | 665876 / 200000 | 93 / 1800 | 18 / 100 |

Even the smallest inspected replacement reported 467555 tokens including cache
reads, so cache reads dominate every dispatched run regardless of task size.
None was promoted to a candidate or given a new check bundle, and the original
two tasks were kept rather than reselected once the definition changed.
Artifact retention is finite: these links were accessible on the observation
date, not permanent archives of complete run telemetry. No inference, workflow
dispatch or configuration change was made.

## Independent check reproduction

Each `check.patch` adds independent regression assertions to the historical test
harness, not production changes or a cherry-picked solution. For each candidate,
`git merge-base --is-ancestor START FIX` exited **0**, with distinct 40-hex
revisions (see its evidence). Task text preserves the historical request including
its issue payload, without new checks or solutions. Generic harness handback
boilerplate is excluded. A Node assertion compared the strings to the downloaded
task artifact (merge-overlap) and decoded User excerpt (source-period), exit **0**.

On a clean checkout at either recorded revision, using an absolute path to the
bundle outside agent context:

```text
git apply --check CHECK.patch && git apply CHECK.patch
cargo test --offline --locked -p board-forge FILTER -- --nocapture
```

FILTER is `eval_merge_overlap_regression` or `eval_source_period_regression`.
Apply exited **0** at all four revisions. Each start ran **one test**, exiting
**101** on its symptom; each fix ran **one test**, exiting **0**. Merge-overlap
also preserves fix/rebase waits; source-period covers malformed-source diagnostics,
closing references and ambiguous/wrong sources. Pins are SHA-256 of patch bytes,
verified by the offline unit test. An outside bundle path alone does not keep
checks and solutions out of agent context: the fixes are in this repository's
history and the bundles will be once merged. The isolation that
[freezing](../docs/eval.md#freezing-the-sample) requires is not yet configured
or verified.

Observed toolchain: rustc 1.93.1, cargo 1.93.1, Linux x86_64. Reproduction needs
the historical repository and Cargo.lock dependencies already cached, including
the pinned safe-outputs Git dependency. `cargo fetch --locked` exited **0** to
prepare the cache; the recorded tests used `--offline`. Bundles make no network
calls and need no credentials, services or inference. They are not repository or
dependency archives. Use a distinct Cargo target per checkout or clean board-forge
between runs: an initial shared-target merge check ran zero tests and was rejected,
then rerun after cleaning. Initial patch-count, missing-cache and compile-timeout
attempts are not fail/pass evidence.

Canonical manifest digest (JavaScript recursively sorted-key spelling):
`4d0f27fca1966cc67dacb3a723f9eb9d5a3b8278aab9da13afcd238911edb00b`.
The parity test pins it and checks the archived pending summary. This identifies
preparation, not a frozen experiment. `runs.json` remains empty; the summary has
no groups or decisions. Arm delivery, resolved models, environment, price basis
and cache proof still belong to the configuration slice.

## Archived 2026-11-03 preflight

The remainder records the earlier gate, not the current sample state.

Imported unchanged in substance from agenthome at
[`ce75a39`](https://github.com/cgwalters-forge/agenthome/blob/ce75a397b285ee000d4046bae405e0ecb6bb08d6/eval/report.md);
only its references and command now point at agent-board. The capacity gate it
records has since been replaced by the protocol's [admission](../docs/eval.md#admission).

Part of [agenthome#52](https://github.com/cgwalters-forge/agenthome/issues/52). As of 2026-11-03, this is **blocked**, not a measured comparison or
an adoption recommendation. No evaluation cells or paid probes were launched.
The first gate is unverified existing weekly capacity and enforceable evaluation
budgets. It is unknown whether new money would be needed; no new money was
requested or authorized. Normal runner defaults and routing are unchanged.

## Authorization and accessible evidence

The run task explicitly authorizes using the
[bounded decision](https://github.com/cgwalters-forge/agenthome/issues/52#issuecomment-6606944805):
the existing OpenAI pool within its existing weekly target, with no new spend.
The retrieved comment's author is `cgwalters-bot`, not cgwalters; authority here
comes from the run task naming it, not from treating bot comments as instructions.
Its last observed update is 2026-11-03T16:33:11Z. Research tasks are restricted to
`cgwalters-forge/agenthome`; configuration and reporting also stayed there.

The [archived preflight.json](https://github.com/cgwalters-forge/agent-board/blob/c7186255164d670ec6a996a7267ecb792f4f892b/eval/preflight.json) retains bounded nonsecret observations and
accessible source links at the starting revision
`b04318245f2a4fb28df8856d7cb809cf990cfea5`. The authorization and all three pinned
source paths were retrieved successfully using `gh api`. The inspected protocol,
configuration and issue comments contain no remaining-capacity ledger or reserved
evaluation allocation. Model prices and the ability to run this session do not
prove spare weekly capacity. No other repository was investigated and no jobs
were dispatched to bypass the gate.

To continue, supply accessible, dated evidence of the existing pool's accounting
window/reset and remaining usable allocation net of commitments, with an evaluation
reservation covering historical sizing, role/cache probes and all 54 cells including
priming. Also supply the aggregate and per-cell enforcement configuration and proof
that dollar, token, turn and elapsed-time limits include every stage/child. The
authorization alone is not that evidence. Lower sized limits may be used, but not
an assumed replenishment or automatic top-up.

## Remaining gates, not completed work

The local executable reports `agentic-job 0.6.9`; this is not a full harness revision
pin or a role-execution test. The
[starting installation documentation](https://github.com/cgwalters-forge/agenthome/blob/b04318245f2a4fb28df8856d7cb809cf990cfea5/README.md#shared-role-templates)
records the source-subagent delivery dependency on **agentic-job#1367** and warns
that rendered files do not demonstrate installation. Current delivery remains
unverified, rather than proved absent by that older documentation. After capacity
is established, pin the actual harness revision and demonstrate that its supported
opencode runner loads and executes the intended role prompts and model settings
through that delivery contract. If absent, stop on agentic-job#1367; do not build a parallel
installer or assume `acp-drive` can spawn arbitrary model children.

Historical sizing/selection was not started after the first gate failed. No
requests, starting commits, independent check bundles or sample digests have been
frozen. Select three real small implements/fix rounds without arm outcomes, retain
their sizing evidence, and prove each independent check fails before and passes
after the historical fix. Keep checks and solutions outside agent context. Freeze
the same immutable requests, starts and check digests for all arms before comparison.

The shared renderer/resolver remains the intended interface: single-strong must
use resolved sol throughout, single-cheap resolved luna throughout, and tiered
the rendered role mapping. Complete single-model bundles are still missing; supply
and test them only as opt-in evaluation bundles, without changing ordinary runner
defaults. Configured IDs are not proof of provider model versions. No exact
model/version maps, complete three-arm bundles, common cost basis, environment pin
or harness pin are claimed here. Archive those with digests before any cell runs.

Cache namespace isolation, identical bounded priming and aggregate enforcement
remain unverified. Once all gates pass, predeclare a balanced rotated order for
three tasks × three arms × three repetitions × two cache strata = **54 cells**.
Include priming within limits, retain every failed/exhausted cell without retries,
and independently check and archive every outcome. Nothing in this report relaxes
the [protocol](../docs/eval.md) or authorizes further spend.

## Offline result and limitations

[manifest.json](manifest.json) deliberately remains pending with null
configuration. [runs.json](runs.json) is the empty record array, not synthetic
measurement data. [summary.json](summary.json) is exactly the output of:

```text
agent-board eval summarize eval/manifest.json eval/runs.json
```

The output has status `pending`, no groups and no decisions. There are **0 of 54**
planned cells, not 54 failed or successful cells. Input/output tokens, cache
read/write tokens, common-basis dollar cost, wall time, stage timings and quality
comparisons are all **unavailable for every arm and stratum** because no experiment
was launched; none are reported as zero usage or zero cost. Preflight session usage
is not an arm measurement and is not included in the empty records.

**quality-cost-time-v1: no measured verdict; blocked before comparison.** Neither
a win nor a no-win nor a measured inconclusive result can be derived from pending
inputs. The summarizer correctly emits no decisions rather than manufacturing one.
Even a future complete 54-cell comparison is a tiny historical sample, not evidence
of general performance or permission to adopt defaults. Synthetic fixture tests
validate the offline machinery only; they are not evaluation evidence.
