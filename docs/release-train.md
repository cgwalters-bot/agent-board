# Release train

> This text is LLM generated.

`agent-board release-train --config FILE [--cache FILE] [--apply]` runs one pass of a release
train: it cuts a release of one Rust repository and then moves the pins of the
repositories that use it, one step a pass. It is tracker's
`.github/release-train/train.cjs` (at 1a550cc) moved here, with the same
decisions, except for the checks before a release, the second listing before
opening a train and the wrappers' commit pins described below. A scheduled workflow in the caller's repository runs it, for example
hourly.

The steps of a train, each a pass:

1. **bump**: a PR on the release repository raising the workspace version in
   `Cargo.toml` and its path packages in `Cargo.lock`.
2. **release**: once it merges into the default branch, and that branch's
   workspace version is still the train's, dispatch the release repository's
   release workflow with `version`. A tracking comment records the dispatch
   first, and only the bot's run of the workflow at the recorded commit counts
   as its run, so a later pass never dispatches it again; a recorded dispatch
   with no run after an hour stalls.
3. **secure-host** (optional, off by default): once the release is out, resolve
   `ghcr.io/cgwalters-forge/secure-host:vX.Y.Z` with `skopeo inspect`, then verify
   the immutable digest with `gh attestation verify`. This stops an unattested
   image, or one built from a different source commit, from being promoted to
   the host. Verification uses the publisher's existing policy: the release
   workflow's certificate identity on `refs/heads/main`, that source ref, the
   reviewed release commit as source digest, and no self-hosted runners. No
   attestation policy, egress or host security boundary changes. A missing tag,
   failed verification or invalid digest stalls and is reported on the tracking
   issue. Open a PR on `cgwalters-forge/actions`, from branch
   `release-train/vX.Y.Z-secure-host`, moving `secure-host/Containerfile.pin` to
   `FROM ghcr.io/cgwalters-forge/secure-host:vX.Y.Z@sha256:…`.
4. **wrappers**: once the release is out (and the optional image PR merged), a PR on the wrappers repository
   moving its pins of the release's workflows to the commit of the release
   tag, checked to be on the default branch, with "`NAME vX.Y.Z`" beside
   them. A pin is never a tag, which could move after its review. A commit
   pin's version is read from the "`NAME vX.Y.Z`" on its line or, failing
   that, from Dependabot's "`# vX.Y.Z`" right after it, which moves with it
   (the release's pins only: beside a caller's pin of the wrappers it is the
   wrappers' tag); pins or versions that disagree stall the train.
   With `secureHost` configured, also move secure-host action references on
   `uses:` lines of the configured wrapper files (strings in a test file are
   left alone) directly to `cgwalters-forge/actions/secure-host`
   at the image PR's merge commit. Before doing so, check that commit is on
   actions' default branch and its image pin matches the release's newly
   resolved, verified digest. Subsequent trains move these direct pins too.
   The release-tag forwarder cannot name the image its release has yet to
   publish; bypassing only that forwarder breaks the cycle. All other release
   workflow/action pins remain on the reviewed release commit.
5. **callers**: once that merges into the default branch, a PR on each caller
   moving its commit pins of the configured wrapper workflows to that merge commit.
   Discover callers across every non-archived, non-fork repository of the
   wrappers repository's owner, not just the configuration's exceptions.
6. **done**: close the tracking issue.

A pass doesn't keep state: it reads the stage from the only open tracking issue (an
issue by the bot titled `NAME vX.Y.Z release train`, carrying the tracking
label) and the PRs on `release-train/vX.Y.Z` from the bot's forks. With no open
train, it follows a release cut outside the train whose version the wrappers
are behind, from step 3; otherwise it opens one when a release is due: at once
after a PR labelled urgent merges, or once `threshold` PRs have merged and
`intervalHours` have passed since the last release. A PR labelled breaking makes
a minor rather than a patch release. Before opening a train it lists the open
trains again without GitHub's creator filter, and opens one only if that read
also finds none of the bot's; otherwise it ends the pass without writing
([tracker#882](https://github.com/cgwalters-forge/tracker/issues/882)).

A pass that finds something a person must fix (a PR closed unmerged or merged
into another branch, a failed release run, a reverted or superseded version
bump, a pin it cannot read, a fork that is not the repository's, a release tag
off the default branch) labels the tracking issue with the
stalled label and says why in a comment; the train waits until the label is
removed. Close the tracking issue to drop a train.

Before deciding callers or done, each pass lists the owner's repositories with
complete pagination, resolves their current default-branch heads, reads their
complete recursive trees, and inspects `.github/workflows/*.yml` and `*.yaml`.
Only actual workflow `jobs.<job>.uses` values naming an exact workflow path in
`wrappers.files` count. Each must use a full 40-character lowercase hexadecimal
commit SHA or `main`; other paths and repositories do not count. A caller at
`main` follows the wrappers on purpose and is never edited, and a repository
with only such callers needs no fork. The train stalls unless `main` is the
wrappers' default branch and has no same-named tag, which GitHub would run in
its place. Pinned callers still get their PRs, but the train stalls instead of
closing while the wrappers' `.github/workflows` at `main` differ from the
merged wrappers bump's, as a later commit may have reverted it; any workflow
change there counts, so check what they run and close the issue by hand. The output
is sorted by repository, file, wrapper path and current pin. Multiple files
and different pins in one repository produce one PR. Already-current files
need no edit or PR.

`callers` entries add extra files, including test fixture strings, and select
fork-name overrides; they never restrict discovery. Files are deduplicated.
An entry with `files: []` only overrides a discovered repository's fork. The
train inventories the bot's forks and reads their parent metadata, so a renamed
fork works without an override. A missing or ambiguous fork, wrong parent,
unreadable repository, truncated tree, incomplete inventory or exhausted request
budget ends the pass with an explicit error, not partial coverage or done. No
fork is created. Existing PRs still have to pass the default-branch, fork and
review checks; stale pins after a merged bump require intervention, not done.

## Preview and apply

Without `--apply` a pass only reads, and prints each decision on a line, each
write it would make as `would: ...`. Outside those changed steps the lines
are the ones train.cjs printed, so the two can be diffed on the same state. `--apply` makes the writes with
`GH_TOKEN`, which must be the bot's: it opens the PRs from the bot's forks,
comments on and labels the tracking issue, and dispatches the release workflow.
A pass makes at most `requestLimit` GitHub API requests, 200 unless configured
(at most 2000). The optional image stage and its
wrapper consumer each invoke at most two external commands (no retry), each
limited by GNU `timeout` to 60 seconds plus 5 seconds to kill. They require
`timeout`, `skopeo` and a `gh` supporting attestation verification on the caller's
runner, registry read access and the existing bot token. No image is run by the
train. Run one pass at a time, for example in a
workflow `concurrency` group without `cancel-in-progress`: passes that overlap
race on the same step, though only one of them dispatches a release.

Discovery shares that budget, including pagination and cache revalidation, and
prints `discovery used N of LIMIT requests` once it completes. A pass that runs
out during discovery fails naming the repository it had reached, or the
inventory or bot forks; raise `requestLimit` past what that implies. The next
pass discovers from the start again: completing coverage in one pass is what
lets it decide callers or done, and the cache saves response bytes, not
requests. Full inventory pages always require the next page. Organization
inventories must match the owner's independently reported `public_repos` plus
`owned_private_repos` counts, including archives and forks before filtering.
Missing private-count visibility or a count mismatch refuses completion rather
than accepting a token-filtered subset. Bot fork inventories have the same check.
This requires the existing credential to expose owner-level repository counts;
the train does not change credentials or request broader permissions. If those
counts are unavailable, deployment remains blocked pending an operator decision.
A user owner is supported only when it is
the authenticated bot, using its private-inclusive inventory; another user's
public-only listing cannot establish complete coverage and is refused.

To avoid stale discovery hiding a caller, tree and workflow representations are
keyed by repository, immutable default-branch commit and credential/representation
identity, and revalidated before reuse on another pass. Within a pass those
immutable reads are memoized. `--cache FILE` optionally persists these disposable
responses using the existing bounded response cache (1,024 entries, 8 MiB,
256 KiB per entry); oversized entries are not retained across passes. Without
it, caching is in memory only. Inventories, identity, branch heads, fork metadata
and write checks remain fresh. Every PR edit rereads its inputs at the current
default-branch head and verifies that only workflow `jobs.uses` values change;
the cache never authorizes a push. Restoring a cache saves response bytes, not
the revalidation requests. Neither discovery nor cache use enables writes:
`--apply` remains off by default.
Discovery also limits response representations to 8 MiB per pass and workflow
editing to 128 reference occurrences per file; exceeding either bound is an
explicit error, not a partial edit. Scalar selection checks parsed YAML so an
identical reference in a `run` string is left untouched.

## Configuration

The file comes from the caller's protected default branch; every key is
required except `secureHost`, `callers` and `requestLimit`, and unknown keys are refused. `callers`
may be omitted or empty. An example with an additive test fixture exception:

```json
{
  "bot": "example-bot",
  "release": {"repository": "example/tool", "fork": "example-bot/tool", "workflow": "release.yml"},
  "tracking": "example/tracker",
  "threshold": 5,
  "intervalHours": 24,
  "labels": {"urgent": "P0", "breaking": "breaking", "tracking": "release-train", "stalled": "needs-human"},
  "wrappers": {"repository": "example/actions", "fork": "example-bot/actions", "files": [".github/workflows/tool.yml"]},
  "callers": [
    {"repository": "example/app", "fork": "example-bot/renamed-app", "files": ["tests/wrapper-pin.txt"]}
  ]
}
```

The shape is train.cjs's `config.json` with `bot` (formerly read from the
poll's `agent-board.json`) added and `pageSize`, `maxRequests` and
`requestTimeoutSeconds` removed: inventory listings paginate in pages of 100
and every request has the client's 30-second timeout. `requestLimit`, a count
from 1 to 2000, replaces `maxRequests`; the cap keeps a mistyped limit from
spending the token's hourly allowance.

### Caller settings

Deployment still requires `cgwalters-forge/tracker` to select a release containing
this change in `.github/workflows/release-train.yml` (`env.AGENT_BOARD_REV` must
be the reviewed commit containing this implementation). In `.github/release-train/config.json`,
remove `.github/workflows/review.yml` from both `callers[].files` lists, retaining
`.github/agentic-job/config-test/tests/config.rs` for tracker and
`.github/agentic-job/test_review.py` for agent-board and their fork overrides.
Existing configurations continue to work before that cleanup. No discovery
enablement key is needed. Optionally append `--cache "$RUNNER_TEMP/release-train-cache.json"`
to the workflow's `agent-board release-train` invocation and restore/save that
file using its existing cache mechanism; no cache wiring is required for correctness.
After deployment run a read-only preview with the bot's token and verify that
the org-site workflow and its real fork are included before applying. The
historical recordings predate discovery and contain no inventory/tree/fork
reads; synthetic extensions exercise discovery, not the deployed org-site
failure or its remediation.

`cgwalters-forge/tracker` sets this optional key in
`.github/release-train/config.json`, asserted by its config test
([tracker#1554](https://github.com/cgwalters-forge/tracker/pull/1554)). It is
supported only for the `cgwalters-forge/agentic-job` release, with the fixed
actions repository and image path:

```json
{"secureHost": {"fork": "cgwalters-bot/actions-1"}}
```

With `secureHost` set, keep `.github/workflows/cgwalters-agent.yml` in
`wrappers.files`: the agent job, and so its secure-host step, moved there from
`cgwalters-agentic-job.yml`. It has no release pin, so without `secureHost` the
wrappers step would stall on it. Each listed file must change through one of the two edits. The
train changes its `uses: ./.agentic-job-source/secure-host` (or an older remote
`cgwalters-forge/agentic-job/secure-host@…` reference) to
`uses: cgwalters-forge/actions/secure-host@<image PR merge commit>` in
`cgwalters-forge/actions`, keeping any comment after it. Keep
`.github/workflows/cgwalters-agentic-job.yml`, `.github/workflows/secure-host.yml`
(its wrapper smoke job) and `lint/tests/review_wrapper.rs` in `wrappers.files`
too; the test file changes only through its release pin, since its secure-host
references are strings, not `uses:` lines. The policy-source checkout and local prepare/run references are not changed.
Update actions' wrapper tests to permit this separate secure-host commit while
continuing to require all other agentic-job pins to match the release commit.
Provision the verification tools in tracker's release-train workflow if absent.

After the image and wrapper PRs merge, validate in a hosted job using that
wrapper: print `agentic-job --version` on the host after secure-host, outside
the agent's container, and compare it with the release. Fixture tests do not
prove which binary a deployed hosted runner has on PATH.
