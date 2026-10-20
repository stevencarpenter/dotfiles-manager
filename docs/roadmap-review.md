# Delivery roadmap review, 2026-10-08

The delivery roadmap targets the personal M5 and work M3 Apple Silicon MacBooks. It requires all four workflows and full replacement ownership. Intel i9 remains in homelab with GNU Stow. Future Linux and desktop Macs are architecture considerations, not delivery gates.

Kaneo DFM is the authoritative backlog. [roadmap.json](roadmap.json) is the reviewed contract and publication snapshot. A populated board does not establish implemented features or machine stability.

Persisted canonical acceptance checkboxes are the sole completion state. Board columns, labels, comments, PR state and snapshot metadata cannot complete work. [The completion contract](checklist-state.md) defines checkoff, prerequisite reads, evidence refresh and safe revisions.

On 2026-10-09, the owner made successful full local validation the publication and merge gate. Hosted CI is informational. This supersedes the historical hosted-check and account-eligibility requirements below and in the roadmap snapshot. Native proof, independent review and action approval remain required. The completion contract records the current policy.

## Review coverage

Eight GPT-6.1-Sol reviewers used low reasoning. Six challenged all 37 original leaves and the aggregate. Two fresh-context reviewers checked the revised 38 leaves and aggregate safety and delivery. Three separate fresh-context graders assessed every suggestion against existing requirements. No reviewer graded their own suggestions.

All 14 suggestions received a disposition: two accepted, nine revised to smaller changes, one grouped duplicate, and two rejected dependency barriers. The JSON snapshot retains task verdicts, scenarios, evidence, proposals, scores, grader reasons and retained edits.

| Review scope | Original leaves checked | Result |
| --- | ---: | --- |
| Evidence and architecture | 7 | Clarify publication evidence closure and provisional device distribution choices. |
| Engine and state | 6 | Validate manager storage as a trust boundary. Keep declaration authoring independent of live path inspection. |
| Providers and profiles | 10 | Existing contracts cover the stated provider scope. Actual inventories and native proof remain required. |
| Four workflows | 7 | Strengthen trial-version protection, exact source pins and refusal of inactive adoption destinations. |
| Cutover and acceptance | 7 | Discover risky native assumptions earlier and reconcile affected evidence after shared changes. |

The aggregate reviews also retained explicit legacy user-file acquisition tests, clarified bootstrap versus durable-state acceptance, and specified realistic fake-provider seams. Real providers are joined at integration. Additional blanket provider and CI dispatch dependencies were rejected because existing completion gates already cover them.

## Strengthened contracts

1. Test shared-source propagation through two synthetic profiles, bounded private overrides, core invariants and duplicate ownership. Shared declarations must be reused rather than copied.
2. Enforce one durable binding through the installed CLI. Preserve receipts and trial baselines on rebinding refusal or concurrent initialization. Validate state and binding destinations before writes.
3. Provide a small reusable-module authoring walkthrough. Errors name the source, resource/field, conflict or failed precondition and corrective action without exposing secret values.
4. Define bounded noninteractive approval and credential failures. Exact reviewed plans still enforce stale-state, privilege, secret and disruption gates. No scheduler or daemon is required.
5. Add one early owner-gated native-discovery leaf. It records unresolved assumptions and adds only scoped blockers. Final exact-candidate native proof remains after integration.

## Evidence and publication checks

The local structural checker verifies unique task keys, valid parent/prerequisite references, an acyclic graph, every leaf reaching final acceptance, and the intended initial readiness frontier. The final plan has five epics, 38 leaves, 76 blocking edges and 11 owner/access gates. Each leaf requires an epic subtask relation; each prerequisite requires a native blocks relation.

`just check` validates documentation fixtures only. Resolver, packaged-workflow and native-device acceptance remain future task evidence. Hosted-CI status in the preserved draft is historical; this review did not recheck GitHub.

Publication completed on 2026-10-08. Full read-back matched all 43 descriptions, statuses, priorities, labels and absent assignees/dates against prepared payloads. Native relation read-back matched all 38 parents, 76 prerequisite edges and two contextual related links (DOT-6 and DOT-2), without duplicates or unexpected edges. All task and related-data pages were checked. The snapshot records exact IDs, ticket numbers and the verification result: zero mismatches.

DFM-1 through DFM-5 are epic scope containers. DFM-6 is the only initial ready-for-agent leaf. DFM-13 remains in-review for the existing publication evidence. Eleven leaves carry needs-human; no implementation or operational task was started. DFM-11 is early native discovery; DFM-37 is final native proof; DFM-43 owns final delivery acceptance.

## Checkbox revision

Three additional fresh-context experts reviewed each change to completion authority, criterion wording, epic coverage, state preservation and persistence. Two separate fresh-context graders assessed the candidate and its revision. All used GPT-6.1-Sol with low reasoning. The retained JSON includes their grades, reasons, corrections and final dispositions.

The 38 leaves now contain 321 canonical requirements with immutable ticket-scoped IDs. The 207 original acceptance entries remain mapped to the resulting criteria. Independent M5/M3 native proof, per-machine workflow demonstrations, operational observation and owner acceptance have separate boxes. Implementation delivery evidence and authorized merge are independently checkable requirements. Documentation work adds no merge gate, and DFM-13 remains evidence-only.

Each epic lists its expected native children and maps every child criterion into its goal or delivery qualifications. Tasks complete through their requirement boxes. Epics complete through all their children's boxes. No second editable summary checkbox can contradict the canonical requirements.

The migration read every live description and relation before writing. It compared descriptions immediately before each update and verified each persisted result. Final readback matched all 43 descriptions, 321 criterion IDs, five complete goal maps, unchanged metadata and all 116 relations. All task and related-data pages were read. There were zero mismatches, zero checked requirements and zero reopened requirements. No completion was inferred from existing status.

A temporary test-only item verified MCP check, uncheck and persistence through an unrelated edit. The original description was restored exactly. Native browser checkbox interaction remains unverified because the browser required sign-in. The MCP has no version-conditional description update, so read-before-write checks cannot eliminate concurrent editing races. No conflicting edit was detected. Repository snapshots retain definitions and verification observations, never authoritative live checkbox state.

## Remaining uncertainty

The manager is not implemented. Private work inputs, actual device inventories and native provider behavior remain required gates. Independent review reduces known roadmap gaps but cannot prove the plan has no further gaps. New findings must extend the ownership ledger, bounded tasks, dependencies and affected integration/native proof before completion.

This task authorizes roadmap and board publication. It does not authorize product implementation, live configuration changes, secret rendering, package retirement, merge, release, billing changes or a Git push.

## Realignment 2026-10-20

The owner audited the daily path on 2026-10-19 and realigned the backlog on 2026-10-20. The direction: fast rolling updates with no soak period, no generation or rollback machinery, and the lighter modular approach (mcp_sync, skills sync, the agent registry and aws-config-generator stay external generators run from the `just sync` wrapper). Three new daily-path leaves: DFM-61 (checkout root in the binding), DFM-62 (one prompt on a TTY), DFM-63 (sync composes configuration, packages, secrets and services). DFM-64 carries the authoring walkthrough from DFM-28. Two leaves from the same day's CI and shell policies: DFM-65 (the gate staged for iteration speed, hosted CI gates merge, PR #57) and DFM-66 (the owner decides the interpreter of bootstrap.sh; its `/bin/sh` choice is a documented security decision).

Superseded whole: DFM-9, DFM-11, DFM-27, DFM-28, each with an evidence comment. Superseded criteria: DFM-19.C02 and C03, DFM-37.C01 to C06, DFM-21.C02, DFM-36.C02, C03, C05, C06, C08 to C12 and C14. Rewritten: DFM-31.C03, DFM-37.C07, DFM-18.C06 and C07, DFM-41.C06 and C11, DFM-21.C01 and C05. Added: DFM-31.C12 and C13, DFM-25.C09. A superseded requirement keeps its line, struck through with a dated note, and counts as complete (docs/checklist-state.md, "Supersede requirements"). `docs/roadmap.json` records every change under `completion_contract.realignment_2026_10_20`.

Leaves DFM-44 to DFM-60, filed after the 2026-10-08 publication, are not in the manifest. The 2026-10-08 verification counts above describe that publication, not the current board.
