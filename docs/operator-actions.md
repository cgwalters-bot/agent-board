# Operator actions

An operator action is a proposal, not permission to write. The schema in
`operator-actions.schema.json` is shared with the review site; the Rust checker
also enforces configured repository scope and comment provenance. This implements
the schema half of tracker#582, not its execution UI.

One unquoted, unindented `agent-board-action` fence holds one object. Its
`schema` is `agent-board/operator-action/v1`, `title` is one line (200 characters),
`why` is a nonblank reason (4,000 characters), and `actions` holds 1–50 mutations
in execution order. Unknown fields and nulls are refused. Numbers are positive
JavaScript-safe integers. Text lengths count Unicode characters.

Every mutation has `type`, `repo` (`OWNER/NAME`) and `number`. The types are:

| Type | Additional required fields | Intended mutation |
|---|---|---|
| `close_issue` | `reason` | Post the reason comment, then close the issue |
| `reopen_issue` | none | Reopen the issue |
| `add_label`, `remove_label` | `label` | Add or remove this exact label |
| `set_project_field` | `project`, `field`, `value` | Set Priority or Status on the item's GitHub project URL |
| `add_sub_issue` | `parent_repo`, `parent_number` | Attach the issue to this parent |
| `move_sub_issue` | `parent_repo`, `parent_number`, `previous_parent_repo`, `previous_parent_number` | Remove from the named old parent, then attach to the new one |
| `approve_pull_request` | `head_sha` | Approve exactly this 40-character lowercase hex head |
| `merge_pull_request` | `head_sha`, `method` | Merge exactly this head using merge, squash or rebase |

Labels and field values are nonblank, at most 200 characters; reasons are at
most 4,000. Project URLs use the existing GitHub project identity rules.
Project numbers have at most 15 decimal digits, without leading zeroes. Integer
numbers accept equivalent JSON spellings such as `1.0` and `1e0`, as JSON Schema
and JavaScript do. Each parent repository must also be on the allowlist. These are typed mutations,
not arbitrary API requests; a consumer must not infer an issue is a PR or vice
versa from its number.

## Check

```sh
agent-board action check "$ISSUE" --config /trusted-checkout/operator-actions.json
```

The repository-owned file, read from a protected default-branch checkout, has
exactly `writers` (operator and bot logins) and `repositories` (explicit allowed
repositories), both nonempty arrays of at most 100 strings. There is no implicit
organisation allowlist, no default configuration and no execution switch. Do not
source this file or its path from proposal content or dispatcher workflow inputs.
To widen repository scope, the operator changes that file through normal review.

The checker reads at most 1,000 issue comments using the job-plan reader and
applies the same newest-writer and foreign-edit rules as [job plans](job-plans.md).
This stops a visitor's copied proposal or a collaborator's edit being accepted
as the operator's proposal; a foreign edit that removes the fence still blocks
fallback. It prints each validated mutation as JSON Lines, preserving order,
and identifies the source comment on stderr. No writes occur.

Checking does not inspect current target state, project membership/options, old
parent relationships or PR heads. The review site must check these before acting,
flag stale actions, use the viewer's credentials, stop on the first failure and
record execution once. Project scope is not write authority: repository scope
alone does not authorize changes to any project. Execution and result markers
are deliberately left to that separate implementation.

`fixtures/operator-actions.json` contains shared accepted/rejected schema cases
and configuration. Both validators should run every case; provenance checks are
separate because they require GitHub comment metadata.

Run `just test-action-schema` with Ajv 8 available on `NODE_PATH` for the
JavaScript schema fixture check, and `cargo test --locked -p board-forge action::`
for the Rust contract and provenance tests.
