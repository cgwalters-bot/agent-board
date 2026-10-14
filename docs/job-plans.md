# Job plans

A job plan is a proposed agent run, written as a comment on the issue it
serves ([#46](https://github.com/cgwalters-forge/agent-board/issues/46)).
Dispatch starts exactly the job the newest accepted plan describes and adds
nothing of its own, so where and how a job runs is a record on the issue rather
than a decision made in the controller.

## Format

A plan is one fenced block with the info string `agent-board-plan`, at the
start of a line, holding one JSON object:

````markdown
```agent-board-plan
{
  "schema": "agent-board/job-plan/v1",
  "repo": "ostreedev/ostree",
  "item": 3665,
  "kind": "implement",
  "runner": "hosted",
  "pool": "claude",
  "brief": "Follow up on the review of ostreedev/ostree#3665: ..."
}
```
````

| Field | Required | Meaning |
|---|---|---|
| `schema` | yes | `agent-board/job-plan/v1` |
| `repo` | yes | Target repository, `OWNER/NAME`; the caller's bounds must allow it |
| `item` | yes | Positive issue number in `repo` |
| `kind` | yes | `implement`, `triage` or `research`: the profile the run gets |
| `runner` | yes | `hosted`, the only runner the caller has |
| `pool` | no | Inference pool named in the caller's `pools.json`; absent leaves it to the caller's variables |
| `brief` | yes | The task, at most 16,000 characters; the run gets it after the kind's profile |

Every field maps onto one input of the caller's
[dispatch.yml](https://github.com/cgwalters-forge/tracker/blob/main/.github/workflows/dispatch.yml)
(`brief` is `task`). Anything else, including `null` for a field, is refused:
the base branch, budget, allowed outputs and what a job stacks on have no
caller input yet, and a plan that names them must not look honoured when it is
not. A `devspace` runner is refused for the same reason; the caller has no
devspace runner for upstream forks yet. Each is added to the schema when the
caller can carry it. One plan per comment; a comment with two is refused.

## Who may write one

A plan counts only in a comment by the operator (`--operator`) or the login the
planning run comments as (`--planner`); anyone else's is ignored, so a visitor
can neither start work nor block it. A writer replaces a plan by replying with
another or by editing one: the plan most recently written or edited wins. If
that plan is malformed, or was last edited by someone other than a writer
(anyone with write access can edit others' comments) or by an account GitHub
no longer names, dispatch stops instead of falling back to the plan it
replaced. A writer's comment edited by someone else counts as such a plan even
if the edit removed its fence. A plan quoted in a reply (`> `) is not a
plan. An issue with more than 1,000 comments is refused, since its newest plan
could be unread.

The planner login is trusted as a whole: any run that comments as it could
write a plan. The caller's bounds still check every dispatched run, so a plan
can ask only for what they allow; a plan is a proposal, not write authority.

## Dispatch

```sh
agent-board dispatch "$ISSUE" --dry-run \
  --caller "$CALLER" --workflow "$WORKFLOW" --operator "$OPERATOR" --planner "$PLANNER"
```

prints the `workflow_dispatch` REST request (method, URL and JSON body) that
would start the caller's `--workflow` at `--ref` (default `main`), and names
the plan's comment on stderr. It needs a token that can read the issue. The
caller, workflow and logins come from the caller's own configuration (its
repository variables), never from a plan or a dispatcher's inputs.

`--apply` sends that request, once per plan. A plan is its comment and the
time that comment last changed, so a writer's edit, or a reply with a plan, is
a new plan. Before sending, dispatch posts one comment on the issue whose
hidden first line names the plan (`<!-- agent-board-dispatch: COMMENT-ID@TIME -->`),
reads the comments again, and goes on only if the plan is unchanged and no
earlier comment names it, so
two dispatches racing on the same plan start one run as long as GitHub's reads
show the comments posted before them; a caller that runs dispatch in one
concurrency group does not rely on that. The comment is then edited to link
the run, or to say why it was not started, and `record=` and (where GitHub
names it) `run=` lines go to stdout.

Records count only from the writers, so a visitor's copied marker does not
block a plan, and `GH_TOKEN` must be a writer's user token (dispatch asks
GitHub whose it is): two accounts that each read only their own records would
both dispatch. A plan with a record is refused, including one whose
dispatch failed or whose dispatcher died between record and request: an
ambiguous dispatch is not retried, and a writer edits or replaces the plan to
try again. Anyone who can delete comments on the issue can remove a record and
so have its plan dispatched again. The run is started with `DISPATCH_TOKEN` if
it is set, else with `GH_TOKEN`.
