# Delivery roadmap review, 2026-10-08

The delivery roadmap targets the personal M5 and work M3 Apple Silicon MacBooks. It requires all four workflows and full replacement ownership. Intel i9 remains in homelab with GNU Stow. Future Linux and desktop Macs are architecture considerations, not delivery gates.

Kaneo DFM is the authoritative backlog. [roadmap.json](roadmap.json) is the reviewed contract and publication snapshot. A populated board does not establish implemented features or machine stability.

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

After filing, fetch every task and native relation. Compare descriptions, status, priority, labels, parents and prerequisite edges with the exact prepared payload. Check all task and related-data pages. Record IDs and any mismatches in the snapshot before calling publication complete.

## Remaining uncertainty

The manager is not implemented. Private work inputs, actual device inventories and native provider behavior remain required gates. Independent review reduces known roadmap gaps but cannot prove the plan has no further gaps. New findings must extend the ownership ledger, bounded tasks, dependencies and affected integration/native proof before completion.

This task authorizes roadmap and board publication. It does not authorize product implementation, live configuration changes, secret rendering, package retirement, merge, release, billing changes or a Git push.
