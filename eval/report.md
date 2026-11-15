# Role-prompt evaluation: blocked preflight for agenthome#52

> This text is LLM generated.

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

[preflight.json](preflight.json) retains bounded nonsecret observations and
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

[manifest.json](manifest.json) deliberately remains pending with no tasks or
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
