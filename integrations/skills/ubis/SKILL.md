---
name: ubis
description: Locate code and document units in this project with the local UBIS index before grepping or opening whole files. Use when looking for where something is implemented or described, or what else to check near code you are editing.
---

# UBIS: find units, not files

```bash
ubis find "<what the code does, or the task title>"   # → path:start-end spans to read
ubis near <path:line | unit-id | Name>                # → callers, callees, siblings, co-changed units
```

Read only the returned spans. Typical flow: `find` with the task, then `near` on the best hit to see what else the change touches.

Results are candidates with evidence (`via`), not proof. The index refreshes itself on every call.
