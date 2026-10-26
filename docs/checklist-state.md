# Kaneo completion contract

Persisted acceptance checkboxes in Kaneo are the sole completion state. Task status, labels, comments, PR state, evidence links and repository snapshots cannot complete a requirement.

## Check requirements

Each leaf has one `## Acceptance criteria` section. The section ends at the next level-two heading. Each canonical requirement is one Markdown task item with an immutable ID, for example:

```markdown
- [ ] DFM-6.C01: Required outcome and its constraints.
- [x] DFM-6.C02: Another required outcome and its constraints.
```

Check a requirement only after its whole outcome has been verified. Record supporting evidence by criterion ID in the ticket or an approved linked artifact. A required failure, unknown or skipped check leaves the requirement unchecked. An approval criterion requires the owner's explicit approval for its named action and scope. A checked box never grants additional authority.

A leaf is complete exactly when its canonical checklist is nonempty, contains every expected criterion once, and every criterion is checked or superseded. IDs must use that ticket's prefix. Missing, duplicate, unexpected, malformed or unreadable criteria prevent a completion claim. Read the live description; do not infer completion from a board column or cached count.

## Supersede requirements

The owner can supersede a requirement instead of delivering it. The line keeps its ID and its unchecked box, its text is struck through, and a note names the date and the reason:

```markdown
- [ ] ~~DFM-6.C03: Required outcome and its constraints.~~ Superseded 2026-10-20: reason.
```

A superseded requirement counts as complete for leaf, epic and prerequisite completion. The box stays unchecked, so a superseded line never reads as verified delivery. Only the owner supersedes a requirement; an agent records the owner's instruction in the note and in `docs/roadmap.json` (`completion_contract.realignment_*`). A whole leaf is superseded by striking every open requirement and moving it to done with an evidence comment. Rule added on 2026-10-20 by owner instruction.

## Complete tasks and epics

Check the leaf's requirements to complete the task. There is no second task completion box. Each epic links its expected children and maps its goal to their criterion IDs. Complete those child requirements to complete the epic. There is no editable epic summary checklist that can disagree with the children.

An epic is complete exactly when its native child relations match its expected child set, its goal map covers its requirements, and every expected child is complete. Missing, unexpected or unreadable children prevent a completion claim. Project completion is derived from all five epics. Derived completion has no separately stored marker or status.

Before dispatch, read the current prerequisite relations and the canonical checklists of all blocking predecessors. Every prerequisite requirement must be checked or superseded. Readiness labels, current evidence and explicit action authorization remain dispatch controls. They are not completion state. Filing tickets does not authorize implementation, merge, live configuration, secrets, retirement, publication or billing changes.

For source work, record exact source identity, successful full local checks, documentation and a verified commit. Changed-source implementation also requires an authorized merge before the leaf is complete. Its separate merge criterion records that outcome. DFM-13 instead requires its preserved validation evidence, accurate PR metadata and reviewed exact-SHA local checks. It does not require merging PR #1.

Validation gate (owner decision, 2026-10-20): the repository is public and hosted CI gates merge. A pull request merges when its `gate` job passed, and the commit hook runs only the fast steps. A criterion that asks for "successful full local checks" is satisfied by a passing `gate` job on the exact source revision or by a local `just check` run. Preserve criterion IDs and unrelated requirements. Do not claim that failed or unstarted hosted checks passed. Native-device proof, independent review and explicit action approval remain separate. This decision replaced the 2026-10-09 gate, which made local validation the merge gate, made hosted CI informational and superseded the hosted-check and account-eligibility requirements of the roadmap snapshot.

## Revise requirements without losing state

Allocate new criterion IDs monotonically. Never renumber surviving criteria or reuse retired IDs. Preserve conditions, prohibitions and evidence qualifications with the outcome they constrain. Keep independent outcomes separately checkable. The roadmap records each original acceptance entry's resulting IDs.

Before editing, read the live description and relations. Preserve markers by ID only for unchanged obligations with applicable evidence. Formatting changes do not reopen work. Materially changed or split obligations start unchecked. Add new requirements unchecked. Record why an affected criterion was reopened. Apply the existing evidence-refresh rule after source, scope or provider changes, and reopen affected requirements. Do not change unaffected markers.

Use conditional writes when available. The current MCP exposes whole-description updates without a version condition. Compare the live description immediately before writing. If it changed, stop that write and reconcile. Read back each result, including unrelated markers and content. These checks detect many conflicts but cannot eliminate a concurrent write between read and update. Avoid simultaneous description edits during a migration.

Update the expected-ID manifest, epic goal map and native relations when scope changes. Never restore live checkbox markers from `docs/roadmap.json` or an older publication snapshot. The manifest stores requirement definitions and relationships, not completion booleans. Recompute affected task, epic and prerequisite completion from fresh checkboxes after a revision.

## Persistence verification

On 2026-10-08, a temporary test-only item verified unchecked, checked and unchecked transitions through the Kaneo MCP. A checked marker survived an unrelated description edit. The test item was removed, and the original description was restored exactly. No real requirement was checked.

Native browser toggle persistence was not verified. The isolated browser required sign-in, and the connection to the existing browser was unavailable. The verified editing path is a saved description update followed by a fresh MCP read. Use this path if the renderer does not expose an editable checkbox. No separate tracker, daemon or automatic status synchronization is required.
