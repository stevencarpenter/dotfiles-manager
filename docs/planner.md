# Planner (DFM-16)

Task: DFM-16. This document records what the first planner does and where it resolves or differs from [docs/architecture.md](architecture.md) (the contract). The contract wins where this document is silent. The resolver is described in [docs/resolver.md](resolver.md) and the ledger in [docs/state.md](state.md).

## What exists

| Part | Location |
| --- | --- |
| Plan types, builder, digest, saved form, stale comparison | `crates/model/src/plan/` (pure, no I/O) |
| Read-only observation of HOME (`lstat`, `readlink`, bounded reads and listings, all descriptor-relative) | `crates/engine/src/observe.rs`, `Dir::peek*` in `crates/engine/src/fsys.rs` |
| `plan` command | `crates/cli/src/plan_cmd.rs` |
| Sensitivity flag on adapters and resolved resources | `AdapterSpec::sensitive`, `ResolvedResource::sensitive` |

Layering follows architecture section 3. `model` takes already-observed facts and returns a plan. `engine` observes. `cli` joins them. No `ports` or `adapters` crate exists yet, so the planner decides from catalog data and observations and runs no adapter. There is no `apply` (DFM-18).

## The command

```
dotfiles-manager plan --root <dir> [--json] [--against <saved plan>]
```

`plan` resolves the bound profile against the trusted sources in `--root` (as `explain` does), reads the ownership ledger, observes every claimed path and its parents under HOME, and prints the plan and its digest. It writes nothing. It starts no process, makes no network call, calls no vault and fetches no source. A test starts provider look-alikes on `PATH` and asserts none runs, and another test asserts that the planner code contains no process, network or write API.

- `--json` prints one JSON document `{"plan_digest", "plan"}` in canonical form (sorted keys), also when the plan is stale or blocked. With `--against` the document has a `stale` array, and an isolated build adds `isolated`. Both sit outside the digest.
- `--against <file>` parses a saved `--json` output, checks that its digest matches its content (an edited file is `E_PLAN_INVALID`), builds a fresh plan and compares. Any difference is `E_PLAN_STALE`, exit 3. Each difference is printed once per kind and subject as `stale <kind>: <subject> was <old> now <new>`, most specific first: `source input`, `ownership`, `observation`, `operation`, `source`, `model`, `binary`. At most 20 lines print, then a line says how many were omitted. The comparison is complete: if the digests differ and no field explains it, one `stale plan: digest` entry says so, so a forged file with a recomputed digest cannot pass.
- The manager stores no plan and no digest (architecture section 10, item 4). The digest is computed on every run.

Exit codes follow section 10: a blocked plan or a stale plan is 3, an unreadable `--root` or saved plan is 2 (`E_REPORT_UNAVAILABLE`, "inability to produce the requested report"), usage errors are 2. A malformed manifest is exit 3 for `plan`. `doctor` must map it to a finding and exit 1 (DFM-35).

## Scope of a plan

DESIGN.md section 7: "Only configuration work belongs in a normal config plan." The plan covers `link`, `render`, `merge`, `assemble` and `distribution` resources. A `package` or `application` resource and a `secret` output appear as `not_in_scope` operations. A secret shows destination and mode only. This matches the `sync` walkthrough in architecture section 13 (secrets `not_in_scope`).

## What a plan contains

Architecture section 9 lists the plan content. This is how each item is met.

| Contract item | In the plan |
| --- | --- |
| binary identity | `binary{version, sha256}` of the running executable |
| binding, machine | `profile`, `machine_id` (the binding digest is audit evidence and is not compared, docs/state.md) |
| resolved model digest | `model_digest`: SHA-256 over resources, values, claims. Values of secret-bearing resources are left out. |
| source identities | `sources[{kind: "path", root, tree}]`: canonical root and tree digest of the loaded source set (section 9, "Path-source identity") |
| observation fingerprints | `observations`: one `Seen` per observed path and one listing per fragment directory. Each operation also lists `preconditions`. |
| ordered operations, prerequisites, privilege | `operations`, in dependency order, each with `after`, `preconditions`, `privilege` (always `user`) and `postcondition` |
| redacted effects | `effect`. A sensitive output shows destination and mode only and has no content hash. |
| ownership | `ownership` per operation, from the ledger |

Operation ids are `<resource>@<path>`, `mkdir@<path>`, `<resource>@scope` and `remove@<resource>@<path>`.

For apply (DFM-18) each operation also carries: `journal_id` (a valid journal id, derived from the model digest, the source trees and the operation id, not from the plan digest, which covers it), `link` (the absolute link target), and `mode` (the mode the resource declares). Apply must read these fields and never parse `effect` or `postcondition`, which are prose. Precondition strings come from `dfm_model::plan::node_state`, and a link's ledger fingerprint is `dfm_model::plan::link_fingerprint` (SHA-256 of the target text). Apply must re-observe with the same requests and use these two functions, so both sides build identical strings. Re-checking them immediately before each write is apply's duty (architecture section 8).

**Removal.** A ledger output that no selected resource claims is planned as `remove` only when the live output matches the ledger fingerprint (`owned`). A changed, foreign, sensitive or directory output is `E_CONFLICT`. An output that is already gone is `current`. Removal is only planned here. Nothing deletes in this task.

## Rules the planner enforces

1. **Names the planner cannot compare.** A claimed path or foreign fragment name with a non-ASCII character is `E_TARGET_INVALID`, because Unicode normalization is not folded and two spellings could be one entry (closing the NFC/NFD gap by refusal, with no new dependency). The rule is conservative: it also refuses names that would be safe.
2. **Manager paths.** A claim that is, lies inside, or contains the state root, config directory, lock file, data directory or cache directory is `E_TARGET_RESERVED`. The planner uses the store's own check (`Store::check_claim`): case-folded text with the U+017F-aware fold (`dfm_model::fold`, one definition) plus filesystem identity, so a link into the manager's storage is caught too.
3. **Claims.** Exact path claims, fragment directory claims (prefix, with the resolver's pattern exception) and distribution leaves come from the resolver. The planner checks them against the machine. A claim, or a parent of it, that differs from an existing entry only by case, as the filesystem reports it, is `E_TARGET_COLLISION`. The check uses the directory listing, so it follows the volume's real behavior. Nothing is planned through an alias, even if the ledger owns the entry.
4. **Parents.** Each parent is read without following links. A link parent is `E_PARENT_UNSAFE` and is named dangling, recursive, or a link that redirects the write. A file where a directory is needed is `E_CONFLICT`. A parent owned by another user, or a deepest existing directory the user cannot write, is `E_PRIVILEGE_UNSUPPORTED`. Missing parents become `mkdir` operations, one per directory, chained by `after`.
5. **Leaf and ownership.** A missing leaf is created. A link that already points at the source is `current`. Anything else needs a ledger entry for this resource and path whose fingerprint matches the live output (`owned`). An unowned, edited, foreign or unverifiable output is `E_CONFLICT` and nothing is replaced. A directory is never replaced. A link fingerprint is the SHA-256 of the link target text. A file fingerprint is the SHA-256 of its content. A sensitive output has no comparable fingerprint (docs/state.md), so an existing one fails closed.
6. **Edges.** Another resource whose output sits directly in a fragment directory and matches its pattern (compared without regard to case) is a producer. The producer's operation is in the consumer's `after`. Edges are per operation. A test asserts that no operation waits for a whole module.
7. **Foreign fragments.** Entries already in a fragment directory that no selected resource produces are recorded as `foreign` fragments with their state. A fragment that appears, disappears or changes makes a saved plan stale. A foreign fragment must be a regular file you own, readable and within 16 MiB. A link, FIFO, directory, unreadable or oversized fragment blocks the consuming resource. A path that a resource claims is never hashed by the fragment pass, so a sensitive output under a fragment pattern is never hashed.
   Unreadable files and directories are `Unknown` facts and give `E_OBSERVATION_UNKNOWN`, exit 3. They do not abort the run.
8. **Cycles.** Operations are ordered with a deterministic topological sort. A cycle is `E_PLAN_CYCLE` and names the operations. This closes the loop that docs/resolver.md says the resolver lets through (two assemblers whose outputs land in each other's directories).
9. **Determinism.** The same resolved model, source tree, ledger and observations give the same plan bytes. Maps are sorted, listings are sorted, and ties in the order are broken by operation id.

A rejected plan still prints. Its `blockers` list gives for each: code, resource, declaring source file, field, path, the conflicting claim or failed precondition, and one correction. No blocker or effect contains a file content or a secret value. The command ends with the first blocker as the error line.

## New error codes

| Code | Meaning | Exit |
| --- | --- | --- |
| `E_CONFLICT` | Named by architecture section 8. An existing output is not owned, was edited, or cannot be verified. | 3 |
| `E_PARENT_UNSAFE` | A parent directory is a link. | 3 |
| `E_PLAN_CYCLE` | Operations depend on each other in a loop. | 3 |
| `E_PLAN_STALE` | A saved plan differs from a fresh one. | 3 |
| `E_PLAN_INVALID` | A saved plan is not valid JSON, has the wrong shape, or its digest does not match. | 3 |
| `E_PRIVILEGE_UNSUPPORTED` | A path needs another owner or write access the user lacks. | 3 |
| `E_OBSERVATION_UNKNOWN` | A required fact could not be observed (too large, unreadable, not observed). | 3 |
| `E_TARGET_RESERVED` | A claim overlaps the manager's own state, config, lock, data or cache paths. | 3 |

`E_TARGET_INVALID` (resolver code) also covers non-ASCII names in the planner.

An unsupported saved-plan schema is `E_SCHEMA_UNSUPPORTED`. A missing state directory is `E_STATE_INVALID`, because ownership would be unknown.

## Architecture ambiguities resolved

| Contract text | Reading used |
| --- | --- |
| Section 10, exit 3: "a required prerequisite that is `UNKNOWN`", and section 8 "An untested probe stays disabled and returns `UNKNOWN`" | A configuration plan has no probe, so no prerequisite is `UNKNOWN` for it. Package and application resources are out of scope (above). A path that cannot be observed is `E_OBSERVATION_UNKNOWN`, exit 3. |
| Section 9, item 1: "content hashes of generated outputs for `normal` class outputs" | No adapter runs yet, so no output is generated. The plan records the identities of the inputs (`inputs`, `source:` preconditions) and the model digest. Output hashes need the adapters (DFM-20). |
| Section 10: `--yes-plan <digest>` "recomputes the plan digest and compares" | `plan --against <saved plan>` is the read-only form. `--yes-plan` belongs to the mutating commands (DFM-18). |
| Section 5, ledger: "exact `owned_outputs` ... with fingerprints" | A link output's fingerprint is the SHA-256 of its target text. DFM-17 left link fingerprints unspecified. |
| DESIGN.md section 4: "A directory link conflicts with every independently claimed descendant" | The resolver checks claims against each other. The planner adds the machine side: a link parent on disk is `E_PARENT_UNSAFE`. |

## Known limits

- **No adapter output.** `render`, `merge` and `assemble` operations are planned as `create_file` or `update_file` without comparing desired content, so a file that already equals the desired content is still planned when the ledger owns it. `already_done` needs the adapters. A `skills/v1` leaf is planned as a link to its source, which is an assumption about that adapter.
- **Generated-output hashes.** Section 9 asks for content hashes of generated normal outputs. They need the adapters (DFM-20). Until then the plan records input identities and the model digest.
- **Prose fields.** `effect` and `postcondition` are free text for people. Apply uses `action`, `path`, `link`, `mode` and `preconditions`.
- **Field claims.** The resolver models path claims only. Claims on fields inside a co-owned document (for example one key of `settings.json`) are not modelled, so two resources writing different fields of one file collide as a path.
- **Apply-time check.** The plan records preconditions. Re-observing and refusing a changed fact immediately before each write belongs to apply (DFM-18, architecture section 8).
- **Mutual-feed assemblers** are rejected as `E_PLAN_CYCLE`.
- **Non-ASCII names** are refused (above), not normalized.
- **Source mode.** The source loader records no executable bit, so a mode-only edit of a source file does not change the tree digest.
- **Sensitive conflicts.** The salted hash is not built (docs/state.md), so any existing sensitive destination blocks the plan, owned or not.
- **No removal.** A ledger entry for a resource that is no longer selected is not planned for release or deletion.
- **No source lock.** The source identity is the loaded tree under `--root`. Lock and origin checks belong to DFM-30.
- **Races.** Observation and the resolver's source read are separate reads. A change between them shows up as a stale plan at the next comparison, not during the run. `apply` must observe again (section 6, step 4).
- **Reserved manager directories** that do not exist yet, and the NFC and NFD collision gap, are unchanged known limits (docs/resolver.md, docs/state.md). The planner does not widen them: it compares case-folded names and the on-disk listing, and it refuses non-ASCII names instead of normalizing them. Reserved manager directories that do not exist yet are covered by the text scope of the store check.
- **Non-UTF-8 names** in a listing, a listing that fails, and a listing over 50,000 entries are three separate reasons (`non_utf8`, `failed`, `too_many`), and each makes the plan fail closed for that directory.
- **Evidence scope.** Tests use the `test-isolation` build and a temporary HOME. No device was used.
