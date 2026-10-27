Maintain digest for 2026-10-23.

```agent-board-maintain-digest
{
  "schema": "agent-board/maintain-digest/v1",
  "date": "2026-10-23",
  "approve": [
    {"pr": "up/proj#7", "head": "1111111111111111111111111111111111111111", "reason": "Dependency bump, lockfile only; all checks green at this head."},
    {"pr": "up/proj#8", "head": "2222222222222222222222222222222222222222", "reason": "Typo fix in docs; all checks green at this head."}
  ],
  "closer": [
    {"pr": "up/proj#9", "head": "3333333333333333333333333333333333333333", "concern": "Changes the lock order.", "minutes": 40,
     "links": [{"path": "src/lock.rs", "start": 12, "end": 30}]},
    {"pr": "other/lib#2", "head": "4444444444444444444444444444444444444444", "concern": "New public API.", "minutes": 25,
     "links": [{"path": "src/lib.rs", "start": 5}]}
  ],
  "waiting": [
    {"pr": "up/proj#10", "reply": "Thanks! Could you rebase on main and add a test?"}
  ],
  "stale": [
    {"pr": "up/proj#3", "reason": "No activity in 90 days."}
  ]
}
```
