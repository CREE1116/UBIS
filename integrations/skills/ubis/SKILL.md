---
name: ubis
description: Locate code and document units in this project with the local UBIS index before grepping or opening whole files. Use when looking for where something is implemented or described, or what else to check near the unit being edited.
---

# UBIS: find units, not files

The project has a local index (`.ubis/index.db`). Prefer it over `grep` + reading whole files.

1. `ubis index .` if the index is missing or files changed (incremental, fast).
2. Describe what you need: `ubis find "<what it does>"`. Results are unit spans: `path:start-end  unit-id`.
3. When you are already looking at a unit, pass it as an anchor:
   - `ubis near <unit-id>` — units that reference it, units it references, siblings.
   - `ubis find --anchor <unit-id> "<refinement>"`.
4. `ubis refs <unit-id>` lists every recorded reference (with resolution mass; `global` origin means name-based, not proven).
5. `ubis open <unit-id>` prints just that span; `--out` zooms to the parent.

Results are candidates, not proof. Open the current file at the given lines before editing; the index may lag the working tree.
Add `--json` for machine-readable output.
