# First TLA+ model: dispatch and the board Run field

**Today's preview/record seam does not prevent duplicate live runs when two
operators or dispatchers execute its output.** TLC finds two valid previews of
the same item followed by two launches, before either run is recorded. This is
not a newly discovered automatic dispatcher bug: the Rust CLI only prints
commands/proposals and has no admission authority. Do not treat its empty-Run
check as a lock. This change does not alter Rust or implement admission.

We follow [gh-aw's work-queue specs](https://github.com/github/gh-aw/tree/main/specs/work-queue),
especially its `check.sh`: a negative control must fail on the **named invariant**,
with TLC's invariant-violation exit code, not merely fail to parse.

## Run it

Install Java (17 or newer), Node (18 or newer) and curl, then from the checkout:

```sh
bash specs/check.sh
```

If Java is not on PATH, set `JAVA_BIN` to its executable. The script downloads
the official TLA+ tools v1.7.4 jar, checks its pinned SHA-256 before executing
it, and leaves logs and state files in a newly printed directory under your
home directory, not in the checkout. No credentials or forge writes are needed.
`check.sh` is a small shell entry point; the standard-library Node harness handles
download verification and result checking. CI runs the same command.

The three configurations share one module, `DispatchJoin.tla`:

- `Preview.cfg` exhaustively checks the CLI alone; it cannot start runs and passes.
- `Current.cfg` includes externally approved execution and recording; it must
  produce a `OneLiveRun` counterexample. **There is no passing current admission
  protocol to model.** This expected failure is retained rather than silently
  substituting the proposed serialized authority in the design document.
- `Broken.cfg` deliberately removes the empty-Run check and combines selection
  with launch. It must also violate `OneLiveRun`; this is the negative control.

The harness exits zero only for this exact combination (TLC exits 0, 12, 12 and
prints the required messages). A parse error, Java failure, unexpected passing
negative control, or checksum mismatch makes the harness exit nonzero. When
admission is implemented, change the current model and expectation together;
do not drop the broken control to make CI green.

## Reading a small specification

A **state** is one complete assignment of the four variables. `phase` says what
each dispatcher has done; `selected` identifies its item; `live` holds actual
run identities per item; `recorded` is the board's Run field. These are functions
(maps), not Rust objects. The two dispatcher names also stand for two distinct
Actions run IDs. All two items initially have empty Run, Todo status and
Coordinator turn, and are open issues with complete fields/schema.

An **action** describes a permitted step between states. An unprimed variable
is its old value; a prime (`live'`) names its next value. A guard such as
`recorded[i] = 0` must be true before the step can happen. `UNCHANGED` explicitly
preserves everything the step does not change. `Next` lets TLC choose any enabled
action for either dispatcher and either item, exploring interleavings rather
than following one favorable schedule. `Init` gives the starting state; `Spec`
says every step follows `Next` or does nothing. Terminal states are permitted:
deadlock checks are disabled because this is a safety question, not scheduling.

Here is the code correspondence (see
[`dispatch.rs`](../crates/agent-board/src/dispatch.rs) and
[the design's preview seam](../docs/design.md#dispatch-preview-integration-seam)):

`Prepare` represents `command()` / `request dispatch`: the snapshot has an open
Todo issue, Coordinator turn and empty Run. Printing argv neither reserves nor
changes the item. We abstract the fixed eligibility prerequisites away, retaining
the empty-Run check as `recorded[i] = 0`.

`Start` represents the operator executing that printed `gh workflow run` argv.
It is outside the Rust CLI. There is no compare-and-claim in the printed command.
The resulting run is live (queued or running), even if its board link is absent.

`Record` represents `proposal()` / `run record --run URL` and external application
of its Run, In Progress and Worker fields. A nonzero `recorded` abstracts those
fields together. The model grants this step an atomic fresh check against
replacing a different Run, **stronger than the current snapshot-based proposal**.
Even that cannot prevent launches that happen before recording.

`OneLiveRun` is the **safety invariant**: every item has at most one live run,
in every reachable state. A single Run text field is not the same thing as a
single actual live run! `BrokenStart` intentionally ignores that distinction's
only existing guard, so it can launch twice in just two steps.

## A real counterexample

With TLC v1.7.4, one worker, seed 1 and fingerprint polynomial 0, the broken
configuration printed this trace (verbatim):

```text
Error: Invariant OneLiveRun is violated.
Error: The behavior up to this point is:
State 1: <Initial predicate>
/\ live = (item1 :> {} @@ item2 :> {})
/\ recorded = (item1 :> 0 @@ item2 :> 0)
/\ selected = (supervisor :> 0 @@ coordinator :> 0)
/\ phase = (supervisor :> "idle" @@ coordinator :> "idle")

State 2: <Next line 64, col 12 to line 65, col 67 of module DispatchJoin>
/\ live = (item1 :> {supervisor} @@ item2 :> {})
/\ recorded = (item1 :> 0 @@ item2 :> 0)
/\ selected = (supervisor :> item1 @@ coordinator :> 0)
/\ phase = (supervisor :> "started" @@ coordinator :> "idle")

State 3: <Next line 64, col 12 to line 65, col 67 of module DispatchJoin>
/\ live = (item1 :> {supervisor, coordinator} @@ item2 :> {})
/\ recorded = (item1 :> 0 @@ item2 :> 0)
/\ selected = (supervisor :> item1 @@ coordinator :> item1)
/\ phase = (supervisor :> "started" @@ coordinator :> "started")
```

Read `:>` as a key/value entry and `@@` as joining entries. In state 2 the
supervisor launched item1. In state 3 the coordinator launched item1 too.
`live[item1]` now has two members: the invariant fails. `recorded` is still empty.

The current configuration's real trace is longer; it preserves the guard but
both dispatchers pass it before either records:

```text
Error: Invariant OneLiveRun is violated.
Error: The behavior up to this point is:
State 1: <Initial predicate>
/\ live = (item1 :> {} @@ item2 :> {})
/\ recorded = (item1 :> 0 @@ item2 :> 0)
/\ selected = (supervisor :> 0 @@ coordinator :> 0)
/\ phase = (supervisor :> "idle" @@ coordinator :> "idle")

State 2: <Next line 60, col 12 to line 61, col 63 of module DispatchJoin>
/\ live = (item1 :> {} @@ item2 :> {})
/\ recorded = (item1 :> 0 @@ item2 :> 0)
/\ selected = (supervisor :> item1 @@ coordinator :> 0)
/\ phase = (supervisor :> "prepared" @@ coordinator :> "idle")

State 3: <Next line 60, col 12 to line 61, col 63 of module DispatchJoin>
/\ live = (item1 :> {} @@ item2 :> {})
/\ recorded = (item1 :> 0 @@ item2 :> 0)
/\ selected = (supervisor :> item1 @@ coordinator :> item1)
/\ phase = (supervisor :> "prepared" @@ coordinator :> "prepared")

State 4: <Next line 62, col 12 to line 63, col 58 of module DispatchJoin>
/\ live = (item1 :> {supervisor} @@ item2 :> {})
/\ recorded = (item1 :> 0 @@ item2 :> 0)
/\ selected = (supervisor :> item1 @@ coordinator :> item1)
/\ phase = (supervisor :> "started" @@ coordinator :> "prepared")

State 5: <Next line 62, col 12 to line 63, col 58 of module DispatchJoin>
/\ live = (item1 :> {supervisor, coordinator} @@ item2 :> {})
/\ recorded = (item1 :> 0 @@ item2 :> 0)
/\ selected = (supervisor :> item1 @@ coordinator :> item1)
/\ phase = (supervisor :> "started" @@ coordinator :> "started")
```

Observed output: Preview passed with 13 states generated, 9 distinct states and
0 left on queue (depth 3). Current exited 12 with 39 generated, 27 distinct and
12 left on queue (depth 5). Broken exited 12 with 6 generated, 6 distinct and
3 left on queue (depth 3). Only Preview completed exhaustive enumeration: failures
stop at the first counterexample, not after checking every reachable state.

## Scope and next steps

This is a bounded model **beside the code, not a proof of the Rust**. It checks
two items and two dispatchers, each launching at most once. Runs never finish
in this model: the counterexample needs only the interval where both remain
live. There are no retries, API failures, partial reads, human field edits,
budgets, slots, authentication, target workflow internals or result links. We
do not claim anything about an external caller's execution concurrency groups;
there is no such guarantee in this repository's printed argv or admission code.
Fresh reads at Prepare and atomic Record favor safety; real stale snapshots or
non-atomic field writes cannot invalidate the exhibited before-recording race.

Writing a claim into Run before launching and reading it back is not a fix on
Projects v2: `updateProjectV2ItemFieldValue` has no expected-value precondition,
so two dispatchers can each read Run empty, each write their own claim, and each
read back their own value before the other's write lands. Without a server-side
compare-and-set, a claim needs one serialized writer, which is the authority below.

The design document proposes one serialized admission authority, durable
reservations, request-key deduplication and conservative ambiguous-dispatch
recovery. Those are future work, not actions in this model. A future model can
test that protocol when it exists. For now, operator approval and verifying
run/item correlation remain necessary, and preview eligibility alone must not
be used to justify two independent automatic dispatchers.
