# Planner

This page records what the planner does where [docs/architecture.md](architecture.md) (the contract) leaves a choice open. The contract wins where this page is silent.

## What exists

| Part | Location |
| --- | --- |
| Plan types, builder, digest, saved form, stale comparison | `crates/model/src/plan/` (pure, no I/O) |
| Read-only observation of HOME (`lstat`, `readlink`, bounded reads and listings, all descriptor-relative) | `crates/engine/src/observe.rs`, `Dir::peek*` in `crates/engine/src/fsys.rs` |
| `plan` command, text summary | `crates/cli/src/plan_cmd.rs`, colors in `crates/cli/src/paint.rs` |
| Sensitivity flag on adapters and resolved resources | `AdapterSpec::sensitive`, `ResolvedResource::sensitive` |

Layering follows architecture section 3. `model` takes already-observed facts and returns a plan. `engine` observes. `cli` joins them, then runs the `render` and `assemble` adapters to fill the output hashes ([docs/adapters.md](adapters.md)). No `ports` or `adapters` crate exists. Apply is in [docs/apply.md](apply.md).

## The command

```
dfm plan [--root <dir>] [--json] [--against <saved plan>]
```

`plan` resolves the bound profile against the trusted sources in the root (`--root`, else the root the binding records; [docs/resolver.md](resolver.md), rule 13). It reads the ownership ledger, observes every claimed path and its parents under HOME, and prints the plan and its digest. It writes nothing, makes no network call, calls no vault and fetches no source. It starts no provider process: its only children are the platform lookups of the user and HOME (`/usr/bin/id`, `dscl`). A test starts provider look-alikes on `PATH` and asserts none runs, and another test asserts that the planner code contains no process, network or write API.

- `--json` prints one JSON document `{"plan_digest", "plan"}` in canonical form (sorted keys), also when the plan is stale or blocked. With `--against` the document has a `stale` array, and an isolated build adds `isolated`. Both sit outside the digest.
- `--against <file>` parses a saved `--json` output, checks that its digest matches its content (an edited file is `E_PLAN_INVALID`), builds a fresh plan and compares. Any difference is `E_PLAN_STALE`, exit 3. See "The `stale` array" below.
- The text mode ends with a `summary:` line, then the `blocked:` lines, then the digest. The summary gives the count of each status that occurs, and `; nothing to change` when apply would write nothing (`Plan::changes_nothing`). The status and id of each `blocked` operation, then of each `planned` one, follow on their own lines, at most 20, then a line says how many more exist. A `not_in_scope` operation is only counted. `apply`, `sync` and the scoped commands print the same plan.
- **Color.** On a terminal the status words, the `summary:` counts, `blocked:` lines and the `result:` line of `apply`, `sync` and `trial` are colored, and so is `error:` on a terminal stderr. Bold red marks what stopped, bold yellow what will change, yellow what waits, green what needs nothing, bold green what changed or ended well, magenta what another scope acts on and cyan what is held. Labels are bold blue, module tags cyan, the digest magenta and `note:` yellow. `NO_COLOR` (any non-empty value) or `TERM=dumb` turns color off. A pipe or file gets no color, and `--json` is never colored. `doctor`, `status` and the apply `counts:` line are not colored.
- The manager stores no plan and no digest (architecture section 10, item 4). The digest is computed on every run.

Exit codes follow section 10: a blocked or stale plan is 3, an unreadable root or saved plan is 2 (`E_REPORT_UNAVAILABLE`), and a usage error is 2. A malformed manifest is exit 3 for `plan`; `doctor` reports it as a finding instead ([docs/resolver.md](resolver.md), "Error codes added by the resolver").

## Scope of a plan

DESIGN.md section 7: "Only configuration work belongs in a normal config plan." A plan has a scope (`Plan.scope`, `plan::Scope`). Each resource outside the scope appears as a `not_in_scope` operation. A secret shows destination and mode only.

| Scope | Command | Resources planned |
| --- | --- | --- |
| `config` | `plan`, `apply`, `sync` | `link`, `render`, `merge`, `assemble`, `distribution` |
| `secrets` | `secrets` | `secret` ([docs/secrets.md](secrets.md)) |
| `packages` | `packages` | `package` ([docs/packages.md](packages.md)) |
| `services` | `services` | `service` ([docs/lifecycle.md](lifecycle.md)) |

An `application` resource is in no scope. The `packages` and `services` plans first ask the providers read-only (they start processes, unlike `plan`). A scope other than `config` plans no removal.

## What a plan contains

Architecture section 9 lists the plan content. This is how each item is met.

| Contract item | In the plan |
| --- | --- |
| binary identity | `binary{version, sha256}` of the running executable |
| binding, machine | `profile`, `machine_id` (the binding digest is audit evidence and is not compared, [docs/state.md](state.md)) |
| resolved model digest | `model_digest`: SHA-256 over resources, values, claims. Values of secret-bearing resources are left out. |
| source identities | `sources[{kind: "path", root, tree}]`: canonical root and tree digest of the loaded source set (section 9, "Path-source identity"). With `[sources]` the loaded set is the declared roots minus the excludes plus the files beside the bundle ([docs/resolver.md](resolver.md), "Source roots and excludes"). |
| observation fingerprints | `observations`: one `Seen` per observed path and one listing per fragment directory. Each operation also lists `preconditions`. |
| managed fingerprint | `managed_sha256` on a merge operation: SHA-256 of the canonical JSON of the declared managed values. |
| ordered operations, prerequisites, privilege | `operations`, in dependency order, each with `after`, `preconditions`, `privilege` (always `user`) and `postcondition` |
| redacted effects | `effect`. A sensitive output shows destination and mode only and has no content hash. |
| ownership | `ownership` per operation, from the ledger |

Operation ids are `<resource>@<path>`, `mkdir@<path>`, `remove@<resource>@<path>`, `<resource>@package`, `<resource>@service` and `<resource>@scope` (a resource outside the plan's scope).

**Fields apply reads.** Each operation carries `journal_id`, `link` (the absolute link target) and `mode` (the mode the resource declares). `journal_id` is derived from the model digest, the source trees and the operation id, not from observations or the plan digest, so a replan after a partial apply gives the same id ([docs/apply.md](apply.md), "Journal ids"). Apply reads `action`, `path`, `link`, `mode` and `preconditions`, and never parses `effect` or `postcondition`, which are prose for people.

Precondition strings come from `dfm_model::plan::node_state`, which compares link targets exactly, so a case-only retarget is a change. A link's ledger fingerprint is `dfm_model::plan::link_fingerprint` (SHA-256 of `link\0` and the target text). A file's is `dfm_model::plan::file_fingerprint` (SHA-256 of `file\0` and the content). It is also the hash an observation records for a file, so a regular file whose bytes are `link\0` and a target is never the owned link. Apply re-observes with the same requests and these functions, so both sides build identical strings.

**Optional fields.** `Operation.output_sha256`, `output_bytes`, `sets`, `fragment_dir`, `excluded` and `Fragment.sha256` are omitted from the JSON when empty. `plan` prints the size and the setting names of a generated file ([docs/adapters.md](adapters.md), "What the plan shows").

**Retire.** A ledger output with a co-owned record (`coowned`) whose resource is no longer selected, at a path nobody claims, is planned as `retire` when the file exists as a regular file of the user. The effect text reads `retire N owned handler(s) in <path> (sensitive: destination and mode only); the file, its managed and seeded values and all foreign entries stay`. A file that is already gone is `current` with `action = remove`, as for any removal.

**Removal.** A ledger output is a removal candidate when its resource is no longer selected and no selected resource claims its path. (A resource that is still selected keeps its outputs even if it claims nothing now.) It is planned as `remove` only when the live output matches the ledger fingerprint (`owned`). A changed, foreign, sensitive, directory, non-ASCII or other-user output is a blocker (`E_CONFLICT`, `E_TARGET_INVALID` or `E_PRIVILEGE_UNSUPPORTED`). An output that is already gone is `current` with `action = remove`, so apply can drop its stale ledger entry. An output whose parent directory is a link is `E_PARENT_UNSAFE` for that entry only, and every other operation stays plannable. A path claimed by a selected sensitive resource is never hashed, and its ledger fingerprint is never printed, whoever owns the entry.

There is no mass-removal guard: deselecting everything plans the removal of every fingerprint-verified owned output. That is by design. The ledger is the only authority, a changed output is never removed, and apply needs one `--allow-disruptive` id per removal.

A fragment that a planned removal deletes is not an input. It is left out of the consumer's `fragments`, the consumer's `fragments:` precondition lists it as `removing=[...]`, and the consumer runs after the removal (`after`). Deselected content never reaches the assembled file, and one apply converges. A blocked or absent removal adds no edge and is not a producer.

**Fragments for a merge.** A `claude-settings/v1` resource that declares `fragments_dir` and `pattern` has a fragment-directory claim, like a `text-fragments/v1` resource. The same fragment pass runs for it: producer edges, foreign fragments with their hashes, the `fragments:` precondition, removals and staleness (rules 6 and 7 below). The listing shows in `Operation.fragments` and in `plan` as `fragments: <names>`. A foreign fragment that is not a plain file of the user blocks the merge, as for an assembly.

The planner stays pure. The CLI reads the fragments for the merge verdict (`merge_facts`), with the same predicate as apply, and the verdict is `Blocked` with `E_CONFLICT` for a fragment that is not a JSON object. A fragment made by a selected link resource is read from its source file. A fragment made by any other kind of resource is `E_GENERATOR_UNAVAILABLE`. A plan that is `current` for a merge has already layered every fragment, so a changed fragment makes it `planned` again.

The fragments of a sensitive merge can hold credentials. The planner does not hash them (the observation is a request with `hash = false`), `Fragment.sha256` is null, and the `fragments:` precondition carries the salted content token from `MergeFact.fragments` ([docs/adapters.md](adapters.md), "Sensitive policy"). `stale` shows a token difference as `fragment content changed`. A fragment writable by group or others blocks the merge. So does a missing `state/salt` when the merge has fragments (`E_STATE_INVALID`). `merge_facts` and the fragment pass must agree on the fragment names. They differ when a deselected producer's fragment is not removed because its removal is blocked. The merge is then blocked with `E_PLAN_STALE` and the fix text names the other blocker.

## Rules the planner enforces

1. **Names the planner cannot compare.** A claimed path or foreign fragment name with a non-ASCII character is `E_TARGET_INVALID`, because Unicode normalization is not folded and two spellings could be one entry. This closes the NFC/NFD gap by refusal, with no new dependency. The rule is conservative: it also refuses names that would be safe.
2. **Manager paths.** A claim that is, lies inside, or contains the state root, config directory, lock file, data directory or cache directory is `E_TARGET_RESERVED`. The planner uses the store's own check (`Store::check_claim`): case-folded text with the U+017F-aware fold (`dfm_model::fold`, one definition) plus filesystem identity, so a link into the manager's storage is caught too. A reserved directory that does not exist yet is covered by the text comparison.
3. **Claims.** Exact path claims, fragment directory claims (prefix, with the resolver's pattern exception) and distribution leaves come from the resolver. The planner checks them against the machine. A claim, or a parent of it, that differs from an existing entry only by case, as the filesystem reports it, is `E_TARGET_COLLISION`. The check uses the directory listing, so it follows the volume's real behavior. Nothing is planned through an alias, even if the ledger owns the entry.
4. **Parents.** Each parent is read without following links. A link parent is `E_PARENT_UNSAFE` and is named dangling, recursive, or a link that redirects the write. One exception: when the link is a ledger entry of a resource that is no longer selected, this plan removes it. The leaf then plans as if the parent were missing: a `mkdir` for that directory runs after the removal, and the leaf runs after the `mkdir`. The own-path check that followed the link is skipped for that leaf.

   A file where a directory is needed is `E_CONFLICT`. A parent owned by another user, or a deepest existing directory the user cannot write, is `E_PRIVILEGE_UNSUPPORTED`. Missing parents become `mkdir` operations, one per directory, chained by `after`. Two claims whose missing parents differ only by case (`a/x` and `A/y`, neither directory present) are `E_TARGET_COLLISION` at plan time, because no listing can show the alias and one `mkdir` would then fail or merge on a case-insensitive volume.

   A missing parent is not an overlap with the manager's own paths: `~/.local/share`, `~/.config` and `~/.cache` may be missing on a fresh machine, although each lies above a reserved directory. The reserved-path check compares the deepest existing prefix by identity and treats the missing remainder as new directories. A `mkdir` path may therefore be an ancestor of a reserved path, but never a reserved path or a path inside one. A claim (a leaf) that is an ancestor of a reserved path is still `E_TARGET_RESERVED`. A link in the existing prefix that leads out of HOME is `E_PARENT_UNSAFE`, not a reserved target.
5. **Leaf and ownership.** A missing leaf is created.

   An existing link that already points at the source is adopted when all of these hold: the resource is a normal-class link or distribution resource, the leaf is a symlink whose target text equals exactly the link the resource would publish (`Operation.link`) and resolves to a file, no ledger entry mentions the path, and every parent is a real directory owned by the user (rule 4). The planner then emits `action = adopt` with status `planned` and ownership `unrecorded_current`. Its preconditions are `path:<leaf>` (the link state, so a change makes the plan stale) and `ledger:<leaf>` equal to `none`, plus the existing parents. It has no `after` edge, because every parent exists, and it produces no fragment. Apply records ownership and writes nothing to HOME ([docs/apply.md](apply.md), "Adoption"). A link that does not meet every condition (a link to a directory, a sensitive resource, a path another resource owns in the ledger) is `current` with ownership `unrecorded_current` and no action. Its effect text says `already holds; not adopted: <reason>`, so `current` does not read as owned.

   Anything else needs a ledger entry for this resource and path whose fingerprint matches the live output (`owned`). An unowned, edited, foreign or unverifiable output is `E_CONFLICT` and nothing is replaced. A link with other text is the plain conflict. A regular file at a link target is a conflict that says it is a file, not the link, even when its content equals the source. The fix is to move the file aside or to replace it with the link by hand; the manager adopts links only, never file content. A directory is never replaced.

   The ledger allows one owner per path. When the ledger names another resource as owner of a missing leaf (a renamed resource, for example), the plan adds the removal of that entry, ordered before the create, and the leaf's `ledger:` precondition is `none`. When the leaf exists and the ledger names another resource, nothing is written: a link or render is `E_CONFLICT` or current. The `plan` command (`merge_plan.rs`) blocks a merge resource whose path is owned by another resource that still claims it. It also blocks when the owner is stale (no longer selected, or selected without a claim on the path) and the file exists, without reading the file; the fix is to move the file aside. When the owner is stale and the file is missing, the plan retires the stale entry first. Moving the file aside then gives the missing case.

   A ledger link entry whose live node is now a real directory is `E_CONFLICT` on removal. Moving the directory aside lets the next plan drop the stale entry. A ledger output that is a link where this resource now generates a file (the resource changed from a link to a render) is `E_CONFLICT`: the manager never updates a file through a link. The reverse is also `E_CONFLICT`: a ledger output that is a regular file where this resource now creates a link is never replaced by a link, because apply cannot replace a file with `replace_link`. A sensitive output has no comparable fingerprint ([docs/state.md](state.md)), so an existing one fails closed.
6. **Edges.** Another resource whose output sits directly in a fragment directory and matches its pattern (compared without regard to case) is a producer. The producer's operation is in the consumer's `after`. Edges are per operation. A test asserts that no operation waits for a whole module.
7. **Foreign fragments.** Entries already in a fragment directory that no selected resource produces are recorded as `foreign` fragments with their state. A fragment that appears, disappears or changes makes a saved plan stale. A foreign fragment must be a regular file you own, readable and within 16 MiB. A link, FIFO, directory, unreadable or oversized fragment blocks the consuming resource. A path that a resource claims is never hashed by the fragment pass, so a sensitive output under a fragment pattern is never hashed. Unreadable files and directories are `Unknown` facts and give `E_OBSERVATION_UNKNOWN`, exit 3. They do not abort the run.
8. **Cycles.** Operations are ordered with a deterministic topological sort. A cycle is `E_PLAN_CYCLE` and names the operations. This closes the loop that the resolver lets through: two assemblers whose outputs land in each other's directories ([docs/resolver.md](resolver.md), "Fragment injection").
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
| `E_PRIVILEGE_UNSUPPORTED` | A path needs another owner or write access the user lacks. Apply also raises it for a leaf that has another owner than the plan found. | 3 |
| `E_OBSERVATION_UNKNOWN` | A required fact could not be observed (too large, unreadable, not observed). | 3 |
| `E_TARGET_RESERVED` | A claim overlaps the manager's own state, config, lock, data or cache paths. | 3 |

`E_TARGET_INVALID` (a resolver code) also covers non-ASCII names in the planner. An unsupported saved-plan schema is `E_SCHEMA_UNSUPPORTED`. A missing state directory is `E_STATE_INVALID`, because ownership would be unknown. A plan or model that cannot be serialized (impossible for these types) is `E_STATE_INVALID`, exit 3, the class `state::canonical` uses for the same case.

## Architecture ambiguities resolved

| Contract text | Reading used |
| --- | --- |
| Section 10, exit 3: "a required prerequisite that is `UNKNOWN`", and section 8 "An untested probe stays disabled and returns `UNKNOWN`" | A configuration plan has no probe, so no prerequisite is `UNKNOWN` for it. A path that cannot be observed is `E_OBSERVATION_UNKNOWN`, exit 3. |
| Section 9, item 1: "content hashes of generated outputs for `normal` class outputs" | The CLI runs the `git-config/v1` and `text-fragments/v1` generators at plan time and records `output_sha256` on each planned `create_file` or `update_file` of those adapters (`plan_outputs`). `dfm-model` stays pure: the model builds the plan, then the CLI fills the hash. The hash is inside the plan digest. |
| Section 10: `--yes-plan <digest>` "recomputes the plan digest and compares" | `plan --against <saved plan>` is the read-only form. `--yes-plan` belongs to the mutating commands ([docs/apply.md](apply.md)). |
| Section 5, ledger: "exact `owned_outputs` ... with fingerprints" | A link output's fingerprint is `link_fingerprint` (above). |
| DESIGN.md section 4: "A directory link conflicts with every independently claimed descendant" | The resolver checks claims against each other. The planner adds the machine side: a link parent on disk is `E_PARENT_UNSAFE`. |

## Known limits

- **Merge output.** A `claude-settings/v1` merge is planned as `create_file`, `update_file` or `none` (current), never with `output_sha256`. It carries `managed_sha256` instead, the digest of the declared managed values, which is not secret ([docs/adapters.md](adapters.md)). The planner never reads a sensitive file. `plan` reads it through the CLI (`merge_facts`) and passes a verdict per resource in `Context.merge`: `Write`, `Current` or `Blocked{code, detail, correction}`. A blocked verdict is a blocker with fix text and no live content. A resource deselected after a merge is planned as `retire`, not `remove`. A `skills/v1` leaf is planned as a link to its source, which is an assumption about that adapter.
- **Generated files already current.** After the CLI computes `output_sha256`, an owned `update_file` whose live content and mode already equal it becomes `current` with action `none`. A rerun then plans nothing. An unowned file with the same content stays a conflict.
- **Generation failure at plan time** (for example a fragment that is not UTF-8 text) is a blocker on the operation with the generator's code. It names the field and the path and carries no content. A fragment produced by a `merge` resource has no hash and is a blocker (`E_GENERATOR_UNAVAILABLE`).
- **Mode on out-of-scope resources.** A bad `mode` on an out-of-scope resource (for example a secret) still raises a blocker. A mode a resource declares outside `0..=0o777` is `E_MANIFEST_INVALID`, not dropped.
- **Sensitive ledger entries.** A path the ledger records as sensitive is never hashed and its ledger text is never printed, whoever claims it now, and a fragment-directory name that matches it in another case is not hashed.
- **Sensitive conflicts.** A sensitive destination of the merge adapter is compared by the salted hash of its managed fields ([docs/adapters.md](adapters.md), "Conflict detection"). Any other existing sensitive destination still blocks the plan, owned or not. `plan` and `doctor` read a merge file and keep the result in memory only. A ledger entry of another resource on the path blocks before the read.
- **Unchecked claims.** A claim that cannot be checked against the manager's own paths (an I/O error, not an overlap) is `E_OBSERVATION_UNKNOWN`, exit 3 (architecture section 10, an unknown required fact). The topology walk reports a link loop or unreadable parent first. The reserved-path and unchecked maps are computed for ledger outputs too, so removal candidates get the same checks.
- **Field claims.** The resolver models path claims only. Claims on fields inside a co-owned document (for example one key of `settings.json`) are not modelled, so two resources writing different fields of one file collide as a path.
- **Source mode.** The source loader records no executable bit, so a mode-only edit of a source file does not change the tree digest.
- **Scoped sources.** With `[sources]`, the digest says nothing about content under an excluded path or outside the roots. A directory link that points at a tree containing excluded content publishes that content on apply, and the plan does not hash it. The `excluded` count covers entries below the directory only; the rules that make a sibling link or a link to a directory with an excluded directory `E_SOURCE_ESCAPE` are in [docs/resolver.md](resolver.md), "Source roots and excludes", item 6. A new excluded entry in that directory makes a saved plan stale.
- **Source identity.** `plan` and `apply` name the loaded tree under the root as a `path` source. They read no source lock. Lock and origin checks run in `sync` ([docs/sync.md](sync.md)).
- **Races.** Observation and the resolver's source read are separate reads. A change between them shows up as a stale plan at the next comparison, not during the run. Apply observes again before each write ([docs/apply.md](apply.md), "Order").
- **Listings.** Non-UTF-8 names in a listing, a listing that fails, and a listing over 50,000 entries are three separate reasons (`non_utf8`, `failed`, `too_many`), and each makes the plan fail closed for that directory.
- **Evidence scope.** Tests use the `test-isolation` build and a temporary HOME.

## The `stale` array

With `--against`, the JSON document has `stale`, a list sorted most specific first, one entry per kind and subject. It is empty or absent when the plan matches. It sits outside the plan, so it is not in the digest.

```json
{"what": "source_input", "subject": "core.git@.config/git/config (core.git)", "was": "<old>", "now": "<new>"}
```

`what` is one of `source_input`, `ownership`, `observation`, `operation`, `source`, `model`, `binary`, `plan`, in that order. `plan` with subject `digest` means the digests differ and no other entry explains it, so a forged file with a recomputed digest cannot pass. The observation entry of a sensitive claim, and of an unhashed fragment of a sensitive merge, reads `file:<mode>` and never shows a size or hash. A content token on either side of a sensitive merge's `fragments:` precondition (a blocked merge has none) reads as `fragment content changed`.

The text mode prints the same entries as `stale <what with spaces>: <subject> was <old> now <new>`, at most 20, then a line that says how many were omitted.
