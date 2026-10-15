# One decisions document

The caller opts in by supplying `decisions` in its trusted default-branch
`agent-board.json`. There is no default repository or issue, and dispatch inputs
must not supply this configuration. The issue must already exist; it may be
private. These commands need issue read access, and `--apply` additionally needs
issue write access. They do not need a project or change board fields.

```json
{"decisions":{"repository":"example/coordination","issue":42}}
```

```sh
agent-board ask add 'Turn on the scheduler?' --context 'The trial passed.' \
  --option 'Yes, now' --option 'Not yet' --recommend 'The trial passed' \
  --item example/work#7 --priority P0 --config agent-board.json
agent-board ask read --config agent-board.json
agent-board ask resolve D1 'Enable after approval.' --config agent-board.json
```

Add and resolve print the proposed body without writing by default. Supply
`--apply` explicitly to write with your token. The older `ask create`, `ask list`
and issue-identity `ask resolve` retain their issue-per-question behavior.
`--snapshot` and `--json` are not supported by the document commands: their
stdout is already JSON. Tests inject an offline GraphQL transport.

The issue body is the interface, without hidden markers:

```markdown
### D1 Turn on the scheduler? (P0, blocks example/work#7)
The trial passed.
- [ ] A) Yes, now ← recommended: The trial passed
- [ ] B) Not yet

Reply:

## Done
```

The operator edits checkboxes, question/context/options, or text after `Reply:`.
Keep the `### D<n>` headings and `## Done` section structurally intact. New
entries go before Done; resolve moves the original entry bytes into Done and
adds `Outcome: TEXT`. It does not rewrite the operator's words. Resolution text
is one line; context cannot contain headings or fences. There are 2–10 options.
IDs are positive, canonical decimal numbers and never intentionally reused:
the next ID exceeds both the current document and the saved high-water mark.

After a successful write, the bot saves a **fenced JSON state file** beside the
configuration: `agent-board.decisions-state.md` (derived by replacing the config
file extension with `.decisions-state.md`). It contains the target identity,
last written body and highest ID observed. This is bot-owned state, not an
operator-editable part of the interface. Persist it between runs. **It contains
the issue body: keep it private, outside public artifacts and source control.**
On Unix, newly published state files are owner-readable/writable only; the
temporary file is created exclusively to refuse existing files or symlinks.
A stale `.md.tmp` file aborts publication; inspect it before removing it.
A read or preview never advances this baseline. `ask read` reports each changed
ID with its before/after text, current ticked lines and exact reply suffix,
including rewordings and deletions. It reports document-only edits as
`other_edits` when there are no changed entries. These are text deltas, not
authenticated attribution or authority to execute the selected option.

When state is absent, the last 100 `userContentEdits` provide a fallback: the
newest edit by the authenticated bot identifies its last written version. Use
the same dedicated bot account between runs; another writer with that identity
is indistinguishable. A missing/redacted baseline fails closed rather than
guessing. An empty issue can be initialized. Writes without local state require
complete, unredacted history to avoid recycling deleted IDs; otherwise restore
the state. Corrected malformed historical structure does not block operation;
history recovery conservatively counts valid ID headings in old versions.
The body and every examined history version are limited to 60,000 bytes.
Malformed headings, duplicate IDs/Done sections and unclosed fences abort.

Writes always merge into the body read from GitHub, not the saved baseline.
Immediately before updating, a second read must match the issue ID, body and
timestamp; a mismatch aborts without retry. This stops an operator edit made
during the merge from being overwritten. **GitHub's `updateIssue` has no atomic
conditional write:** an edit between that final read and the mutation can still
race. Serialize bot writers and avoid simultaneous human edits during apply;
this command does not claim atomic compare-and-swap. A confirmed write is
followed by atomic local state replacement. If state persistence fails after
the issue write, the command reports failure; inspect the issue before retrying.
No scheduler, workflow, or automatic answering is enabled.
