Session on cgwalters-forge/agent-board, 2026-11-09: 14 open issues and 3
pull requests read once. One is done, one is wider than its slice, one has
no Priority, one belongs to the maintainer epic, one is ready for its next
slice, and the pools' size is the operator's call.

```agent-board-session
{
  "schema": "agent-board/session-batch/v1",
  "repo": "cgwalters-forge/agent-board",
  "actions": [
    {"type": "close_issue", "issue": "cgwalters-forge/agent-board#701",
     "reason": "Done by #788: prompts live in each repository now, as this asked."},
    {"type": "rescope_issue", "issue": "cgwalters-forge/agent-board#683",
     "body": "Make the poll's merge-queue read one page, within the pass's request budget.\n\nThe rebase half moved to #691.",
     "reason": "The rebase half landed with #691; what is left is the one-page read."},
    {"type": "set_priority", "issue": "cgwalters-forge/agent-board#691", "value": "P1"},
    {"type": "attach_to_epic", "issue": "cgwalters-forge/agent-board#703",
     "epic": "cgwalters-forge/tracker#1356"},
    {"type": "post_plan", "issue": "cgwalters-forge/agent-board#830",
     "plan": {"schema": "agent-board/job-plan/v1", "kind": "implement", "runner": "hosted",
              "brief": "Implement the second slice of agent-board#830: apply an accepted session batch once, with a receipt on its item."}},
    {"type": "ask_operator", "domain": "budget",
     "question": "May the claude pool's burst rise to 120 while sessions run daily?"}
  ]
}
```
