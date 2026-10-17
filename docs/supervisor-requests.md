# Supervisor requests

`supervise execute` treats the supervisor's comment as attacker-controlled data.
It accepts one `supervisor-actions` fenced block containing JSON Lines, at most
ten requests and 32 KiB total. Every line has `"type":"supervisor_action"`.
Unknown actions, unknown fields and incomplete evidence refuse the whole batch
before any write, including its reservation in the log. No request can select an
API endpoint, configuration file, credential, assignee or arbitrary label.

The existing trusted supervisor configuration's `agent` object may additionally
contain:

```json
{
  "dailyCaps": {
    "comment": 4,
    "create_issue": 2,
    "close": 2,
    "remove_needs_human": 1,
    "replace_state": 4,
    "status_update": 1
  },
  "tracker": "OWNER/tracker",
  "state": {"repository": "OWNER/tracker", "issue": 123},
  "project": "PVT_CONFIGURED_NODE_ID"
}
```

All new daily caps default to zero (disabled), accept integers from zero to 100,
and `status_update` accepts at most one. Dispatches of all admitted kinds share
the existing `maxDispatchesPerPass` and `maxDispatchesPerDay`, both defaulting to
zero; `agent.kinds` must explicitly enable each kind. The operator, bot,
dispatcher/workflow, `follow` repositories and epics repository still come from
`--config`; requests, log, state, tracker, project and caps come from
`--supervisor-config`. These files must be the caller's trusted default-branch
files, never agent-provided files or workflow inputs. Tracker and state must
belong to `follow`; the project is a trusted Projects v2 node ID, not a request
field. State must be distinct from requests and log.

## Lines

The examples below show all fields for each action; none accepts extra fields.

```jsonl
{"type":"supervisor_action","action":"dispatch","repository":"OWNER/repo","item":1,"kind":"triage","pool":"small","reason":"no-plan"}
{"type":"supervisor_action","action":"comment","repository":"OWNER/repo","item":1,"body":"Investigation and links."}
{"type":"supervisor_action","action":"create_issue","title":"Decision needed","body":"Question and evidence."}
{"type":"supervisor_action","action":"close","repository":"OWNER/repo","item":1,"triage_run":"https://github.com/OWNER/tracker/actions/runs/456","triage_comment":789}
{"type":"supervisor_action","action":"remove_needs_human","repository":"OWNER/repo","item":2}
{"type":"supervisor_action","action":"replace_state","body":"Arbitrary memory, including structured state."}
{"type":"supervisor_action","action":"status_update","status":"on-track","body":"Project progress."}
```

Dispatch uses the configured pool allowlist and kind-to-pool assignments, and
the fixed reasons/brief in the playbook. Triage and research require an open,
locked, non-held ordinary issue with collaborator-only authors/editors and no
run in flight. Implement requires those checks **and** native parentage under an
open `epic`-labeled issue in the configured epics repository. A body link claiming
approval does not suffice. The `epic` label remains the operator's approval;
this executor cannot add it or create parentage.

Comments target issues or pull requests in `follow`. New issues go only to
`tracker`, assigned to the configured operator and labeled `needs-human`.
Titles are nonempty and at most 256 characters; public bodies are nonempty and
at most 4,000 characters. They cannot contain triple-backtick fences, HTML
comment markers or the triage-done receipt phrase below: an injected request
must not forge a bot-authored plan, poll marker or closure receipt. State alone
allows arbitrary text (including empty memory), bounded by 60,000 characters
and the batch byte limit. Its action replaces only the configured issue body.
Status accepts `on-track`, `at-risk` or `off-track` and publishes the supplied
body using the same Projects v2 mutation as `project status-update`.

A comment is the agent's text posted as the bot, so the executor appends the
fixed hidden note `<!-- agent-board-supervisor-note -->`. Both the supervisor's
dispatch preflight and the scheduler's collaborator-only input check refuse a
conversation holding it. A batch cannot comment on and dispatch the same item,
and a comment is refused while a dispatch run on its target is in flight.

Closing requires both a successful completed triage run of the configured
dispatch workflow targeting this exact issue and an unedited comment by the
configured bot on that issue. It must contain exactly one `schedule-triage`
fenced JSON block, parsed like tracker's `.github/poll/schedule.cjs` `readTriage`,
with a `done` verdict, for example:

````text
Completed, with links to evidence.
```schedule-triage
{"verdict":"done"}
```
````

Fences are case-sensitive and start in column zero, with optional trailing
spaces or tabs. Prose and other fields (including a scheduler `request`) are
allowed; malformed JSON, multiple matching blocks, or any other verdict refuse.
The supervisor's triage dispatch brief asks for this answer. The named run and
issue bind it here, rather than a scheduler question's request ID.

The comment must have been created within that run's start/end timestamps.
The finding must still describe the issue: no reopen or rename event, body
edit, or comment may follow the receipt, except the bot's own comments before
the triage run ended.
`triage_comment` names its comment ID, not an issue number. A run link alone is
not a done finding. The old exact-body `Triage found this issue done: URL`
receipt is no longer accepted.

Removing `needs-human` is only for an open pull request of the bot's: the newest
label-add event must be the bot's, preceding its newest unedited poll stop
marker on the same head. Current review must be approved, checks successful,
the pull request not a draft, and its mergeability conflicting. Missing facts,
another head, a human's later label event, or any other failed check leave the
hold alone. The poll now retains the stopped head in its marker; older markers
without that evidence remain ineligible. Check contexts must be complete, and
the poll's required-check evaluation also reads both rulesets and classic branch
protection, including missing required checks and expected integrations.
Provenance and current facts are reread immediately before removal.
No other label change is supported.

## Accounting and limits

Epics and the requests, log and state issues are never ordinary request targets.
Only `replace_state` can edit state. The executor never merges, enqueues,
changes variables, reads secrets, edits other issue bodies, or closes pull
requests. The existing first-attempt gate and stop-on-first-write-failure apply
to every kind. A failed or interrupted write consumes its reserved cap, because
it may have reached GitHub.

The rolling 24-hour counts come only from bot-authored executor entries on the
configured log issue. All requests are recorded as `starting` before any action
and their outcomes are recorded as before. No user-supplied text is copied into
those accounting lines. Comment/evidence pagination and the log census fail
closed at their existing finite page limits. The caller must serialize executors
against the same log, as the hosted execute job already does; this is not an
atomic transaction with concurrent human edits or other controllers. Project
updates by other tools do not consume the supervisor's daily cap.
