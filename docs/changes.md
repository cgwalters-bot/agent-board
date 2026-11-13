# Changes

> This text is LLM generated.

`agent-board changes` lists what changed in the followed and midstream
repositories since each one's watermark, read-only. It is the first step of
a change-driven pass (agent-board#894): a pass that starts from the diff
rather than from every open item again. Today it is a standalone command;
nothing in the pass consumes it yet.

With `--since`, the command is instead the earlier list of what landed,
closed, opened or changed hold on the live project, which
`project status-update` posts ([CLI reference](cli.md#agent-board-changes)).

## Running it

From a caller checkout, with no flags:

```sh
agent-board changes
```

reads `.github/pass.json` (`--config FILE` names another), takes the token
from `GH_TOKEN`, `GITHUB_TOKEN` or `gh auth token --hostname github.com` as
the pass does ([local credentials](pass.md#local-github-credentials)), and
keeps its state in `changes.json` under `$XDG_CACHE_HOME/agent-board` or
`~/.cache/agent-board` (`--state DIR` names another directory). The output is
a summary of at most 20 lines: the totals, then a line per changed repository
with its items counted by change, operator comments and reviews counted
last. `--json` prints the whole diff, with `--json schema,repositories` and
`--jq` selecting from it as elsewhere.

The watermarks move only with `--commit`: without it, the next run lists the
same changes again, reading their detail again. The first read of a
repository covers the last 24 hours, and every read before its commit the
same window: the state keeps where it started, so that a later run does not
narrow it to its own clock and drop changes the earlier run listed. A read
after a commit starts five minutes before the watermark, for clocks that
disagree, so a change near the watermark may be listed twice rather than
never. State is replaced atomically only after the output is generated and
flushed successfully; invalid JSON selections, jq errors and output failures
do not advance watermarks.

`--record FILE` saves a live read as a replay fixture and `--replay FILE`
reads one instead of the forge, with no token, as the pass's do; a replay
needs `--state`, so that a fixture's watermarks never land in the cache.

## Repositories

Every repository of the configuration's `follow` list, whatever its
visibility (a private repository is read only when named there), plus the
midstream forks of the `midstream` owner, discovered from the organization's
public listing with no allowlist: a fork (GitHub's `parent`, as the listing
reports it), neither archived nor private, with the configured topic or the
custom property `origin: fork`. Without a `midstream` section, only the
`follow` list is read.

## What is listed

For each issue or pull request updated since the watermark:

- opened, closed, reopened, merged, edited (body or title) and labelled;
- comments, the configured `operator`'s flagged;
- reviews and review dismissals;
- pushes to a pull request, and the check state of an open pull request's
  head as GitHub rolls it up;
- sub-issue and parent links;
- project membership and Status changes;
- converted to a draft or marked ready for review.

Review dismissals count as `reviewed`, with a separate `review_dismissals`
list in JSON: the actor, time, event URL and previous review state, plus the
review URL and reviewer when GitHub still exposes them. The operator flag
on a dismissal identifies who dismissed it, not who wrote the review. A
dismissed approval is not a fresh approval; consumers must re-read current
approval and check state before acting. This adds no writes or pass dispatch
and uses the existing one-page timeline bound.

A pull request whose head checks were still running is read again on later
runs until they settle, so that their result is a change too (`checked`),
although it updates nothing GitHub lists; at most a hundred are followed at a
time, including settled results not yet acknowledged with `--commit`. A
settled result remains replayable until commit, even after the pull request
ages out of the update window. Pending entries outside the current repository
scope do not consume its cap, but are kept for other caller checkouts sharing
the state directory. An item whose update the timeline does not explain is listed as
`updated`.

## Reads

Each repository costs one conditional REST read of its issues and pull
requests updated since the watermark, which GitHub answers with 304 when
the list is as it was and does not count against the rate limit; the
summary's `revalidated` counts those, and the state keeps the numbers the
list held, which a 304 confirms. A list of more than one page, or of one
full page, is read whole each time, as a later page's change leaves the
first page's ETag as it was, and a full page can gain a successor the same
way.
The numbers listed are then read in detail, 25 to a GraphQL request, with
their timeline since the same time. A run after a commit with nothing
changed reads one list per repository and nothing else; the recorded
fixture under `fixtures/changes/` shows both runs.

A repository with more than ten pages, a thousand updated items, or an
item with more than a page of timeline events in the window, refuses the
read rather than report part of it.

## Limits

- A Project field change alone, such as a Priority edit, does not update the
  item on GitHub and so does not surface it; Status changes are listed only
  on items that changed otherwise.
- A check result on a pull request the diff never saw pushed is not read.
- Nothing consumes the diff yet: the pass's stages, the activity score of
  per-repository sessions and the summary's `changes: … → actions` line of
  agent-board#894 are later steps.
