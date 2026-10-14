# Epic plans

An epic plan is a coordinator's proposed changes to an epic's sub-issues,
written as a comment on the epic (slice 2 of
[tracker#572](https://github.com/cgwalters-forge/tracker/issues/572)). Like a
[job plan](job-plans.md) it is a proposal: agent-board checks it against bounds
from the caller's configuration, and only those bounds limit what it may change.
`epic plan --dry-run` prints the GitHub mutations a plan would make; applying
them is a later slice.

## Format

One fenced block with the info string `agent-board-epic-plan`, at the start of
a line, holding one JSON object:

````markdown
```agent-board-epic-plan
{
  "schema": "agent-board/epic-plan/v1",
  "epic": "cgwalters-forge/tracker#572",
  "actions": [
    {"type": "create_sub_issue", "repo": "cgwalters-forge/agent-board",
     "title": "epic apply", "body": "...",
     "plan": {"schema": "agent-board/job-plan/v1", "kind": "implement",
              "runner": "hosted", "brief": "..."}},
    {"type": "set_priority", "new": 1, "value": "P1"},
    {"type": "move_to_ready", "new": 1},
    {"type": "close_sub_issue", "issue": "cgwalters-forge/tracker#573", "reason": "..."},
    {"type": "rescope_sub_issue", "issue": "cgwalters-forge/agent-board#63",
     "body": "...", "reason": "..."}
  ]
}
```
````

`epic` must be the issue the plan is on. Each action is one of:

| `type` | Fields | Mutations |
|---|---|---|
| `create_sub_issue` | `repo`, `title`, `body`; optional `labels`, `plan` | Create the issue, add it as a sub-issue of the epic, and post `plan` on it as a job plan |
| `close_sub_issue` | `issue`, `reason` | Comment the reason, close as not planned |
| `rescope_sub_issue` | `issue`, `body`, `reason` | Replace the body, comment the reason |
| `set_priority` | `issue` or `new`, `value` | Set the board's Priority |
| `move_to_ready` | `issue` or `new` | Set the board's Status to Ready |

`issue` is `OWNER/REPO#N`; `new` is the 1-based position of an earlier
`create_sub_issue` in `actions`. A job plan in `plan` has every field of a job
plan except `repo` and `item`, which are the new issue's. In the dry run's
output the new issue's number is `NEW-k`, and project fields are named by field
and option, which apply resolves to node IDs.

## Refusals

The plan is refused if any action is, and every refused action is named with
its position and reason. Only the epic the plan is on is protected as an
epic: a sub-issue that is itself an epic is treated as any sub-issue. Refused are:

- an issue that is neither new nor a current sub-issue of the epic (read from
  GitHub at check time), and a closed sub-issue, which a plan may not revive;
- a repository not given with `--allow-repo`;
- any change to the epic itself, whose text, priority and labels are the
  operator's, and an `epic` label on a new issue, which would let the planner
  start epics of its own;
- a Priority not given with `--priority-option`;
- more moves to Ready than `--ready-cap`, which is 0 unless set;
- unknown fields and action types, anywhere, including in a job plan;
- two actions of one type on one issue, or any other action on an issue the
  plan closes;
- a `reason` holding an `` ```agent-board `` fence or a `<!--` marker: it is
  posted as a comment by the applier, a plan writer, where it would count as a
  plan or a dispatch record;
- more than 20 actions, titles over 256 characters, bodies over 60,000 and
  reasons over 4,000, and empty text.

## Who may write one

As for job plans: only the operator's (`--operator`) and the planner's
(`--planner`) comments count, the newest written or edited wins, and a newest
plan that is invalid or last edited by anyone else stops the check rather than
falling back to an older one. See [job plans](job-plans.md#who-may-write-one).

## Dry run

```sh
agent-board epic plan "$EPIC" --dry-run --operator "$OPERATOR" --planner "$PLANNER" \
  --allow-repo OWNER/REPO ... --priority-option P0 ... --ready-cap N
```

prints one JSON object a line per mutation, each tagged with its action's
position, and names the plan's comment on stderr. It needs a token that can
read the epic. The bounds come from the caller's own configuration (for
tracker, `schedule.repositories` in its `agent-board.json`), never from a plan
or a dispatcher's inputs.
