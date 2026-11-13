# Release train

> This text is LLM generated.

`agent-board release-train --config FILE [--apply]` runs one pass of a release
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
   With `secureHost` configured, also move secure-host action references in the
   configured wrapper files directly to `cgwalters-forge/actions/secure-host`
   at the image PR's merge commit. Before doing so, check that commit is on
   actions' default branch and its image pin matches the release's newly
   resolved, verified digest. Subsequent trains move these direct pins too.
   The release-tag forwarder cannot name the image its release has yet to
   publish; bypassing only that forwarder breaks the cycle. All other release
   workflow/action pins remain on the reviewed release commit.
5. **callers**: once that merges into the default branch, a PR on each caller
   moving its commit pin of the wrappers to that merge commit.
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

## Preview and apply

Without `--apply` a pass only reads, and prints each decision on a line, each
write it would make as `would: ...`. Outside those changed steps the lines
are the ones train.cjs printed, so the two can be diffed on the same state. `--apply` makes the writes with
`GH_TOKEN`, which must be the bot's: it opens the PRs from the bot's forks,
comments on and labels the tracking issue, and dispatches the release workflow.
A pass makes at most 200 GitHub API requests. The optional image stage and its
wrapper consumer each invoke at most two external commands (no retry), each
limited by GNU `timeout` to 60 seconds plus 5 seconds to kill. They require
`timeout`, `skopeo` and a `gh` supporting attestation verification on the caller's
runner, registry read access and the existing bot token. No image is run by the
train. Run one pass at a time, for example in a
workflow `concurrency` group without `cancel-in-progress`: passes that overlap
race on the same step, though only one of them dispatches a release.

## Configuration

The file comes from the caller's protected default branch; every key is
required except `secureHost`, and unknown keys are refused. An example, with placeholder names:

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
    {"repository": "example/app", "fork": "example-bot/app", "files": [".github/workflows/review.yml"]}
  ]
}
```

The shape is train.cjs's `config.json` with `bot` (formerly read from the
poll's `agent-board.json`) added and `pageSize`, `maxRequests` and
`requestTimeoutSeconds` removed: listings are one page of 100, the request
bound is fixed, and every request has the client's 30-second timeout.

### Caller settings

For `cgwalters-forge/tracker`, add this optional key to
`.github/release-train/config.json` and extend its config test to assert the
fork. It is supported only for the `cgwalters-forge/agentic-job` release, with
the fixed actions repository and image path:

```json
{"secureHost": {"fork": "cgwalters-bot/actions-1"}}
```

Keep `.github/workflows/cgwalters-agentic-job.yml` in `wrappers.files`: the
train changes its `uses: ./.agentic-job-source/secure-host` (or an older remote
`cgwalters-forge/agentic-job/secure-host@…` reference) to
`uses: cgwalters-forge/actions/secure-host@<image PR merge commit>` in
`cgwalters-forge/actions`. Keep `.github/workflows/secure-host.yml` (its wrapper
smoke job) and `lint/tests/review_wrapper.rs` in `wrappers.files` too. The
policy-source checkout and local prepare/run references are not changed.
Update actions' wrapper tests to permit this separate secure-host commit while
continuing to require all other agentic-job pins to match the release commit.
Provision the verification tools in tracker's release-train workflow if absent.

After the image and wrapper PRs merge, validate in a hosted job using that
wrapper: print `agentic-job --version` on the host after secure-host, outside
the agent's container, and compare it with the release. Fixture tests do not
prove which binary a deployed hosted runner has on PATH.
