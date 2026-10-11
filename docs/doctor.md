# Doctor and status (DFM-35, files and state slice)

Task: DFM-35. This document records what `doctor` and `status` do in this build and where it differs from [docs/architecture.md](architecture.md) (the contract). The contract wins where this document is silent. Related: [planner](planner.md), [apply](apply.md), [state](state.md), [resolver](resolver.md).

## What exists

| Part | Location |
| --- | --- |
| Row type, code registry, exit rule, table and JSON output | `crates/model/src/doctor.rs` |
| The shared inspection and the two commands | `crates/cli/src/doctor_cmd.rs` |
| Plan building from an already resolved profile (`plan_for`) | `crates/cli/src/plan_cmd.rs` |

`model` stays pure. No dependency was added.

## The commands

```
dotfiles-manager doctor --root <dir> [--json]
dotfiles-manager status --root <dir> [--json]
```

Both run one inspection and show the same rows. `doctor` prints `detail` and `fix` for each row. `status` prints the status line and the subject only. `--json` prints one document (`schema: 1`) with the same rows for both commands.

Both commands are read-only. They run no program, probe no tool, version or PATH entry, fetch nothing, authenticate nothing, reload nothing, write nothing and take no lock. State is opened with the read-only store open. The lock file is never created.

## Exit codes

| Exit | Meaning |
| --- | --- |
| 0 | No row is ERROR and no required row is UNKNOWN. A WARN row is listed and does not change the exit. |
| 1 | Any ERROR row, or any UNKNOWN row of a required check. The process prints `E_DIAGNOSED` on standard error. |
| 2 | Usage error (missing `--root`, unknown flag, repeated flag, `--against`), or no report can be made at all. |

`doctor` never exits 3 or 4 (architecture section 10). A malformed manifest, a missing profile or an unreadable state file is a row, and exit 1.

## Row fields

| Field | Meaning |
| --- | --- |
| `kind` | `binding`, `state`, `lock`, `profile`, `module`, `resource`, `output`, `intent`, `receipt` or `plan`. |
| `code` | A stable code. Either an `E_*` code that already exists, or a `D_*` code from the table below. |
| `status` | `OK`, `WARN`, `ERROR`, `UNKNOWN` or `N/A`. |
| `subject` | The path, file or resource. Output paths and the stray state entries are relative to HOME or to the state directory. The subjects of the `binding`, `profile` and `lock` rows are absolute paths. A subject is cut at 4096 characters. |
| `detail` | What was found. It never holds a file content or a secret value. |
| `fix` | One line of fix text. |
| `required` | `true` unless the row is a check this build cannot run. An UNKNOWN required row is exit 1. |
| `partial` | `true` when the row comes from an inventory that was read in part. |

All text passes through `dfm_model::display::visible`. Neither format has an ANSI escape or a raw control character.

## Row catalogue

| Kind | When | Status and code |
| --- | --- | --- |
| `binding` | The binding file | OK `D_OK`. ERROR `E_BINDING_INVALID` (unbound, unreadable or unsafe). |
| `state` | `state.json`, `ledger.json`, `journal.jsonl` | OK `D_OK`. ERROR with the document's own code (`E_STATE_INVALID`, `E_IDENTITY_MISMATCH`, `E_SCHEMA_UNSUPPORTED`, and so on). |
| `state` | The state directory | ERROR `E_STATE_INVALID` for state loss (binding without state) or state without a binding. ERROR `E_STORAGE_INVALID` for a redirected or badly owned directory. N/A `D_STATE_ABSENT` when neither exists. |
| `state` | A receipt or trial that cannot be read, or belongs to another identity | WARN with the file's code, labeled partial. Receipts are audit evidence and never authority. |
| `state` | Stray entries (interrupted writes, staging directories) | WARN `D_STATE_PARTIAL`, labeled partial. |
| `intent` | A journal intent with no outcome | ERROR `E_OP_INTERRUPTED`. A running apply also leaves an intent until it ends, and doctor cannot tell the two apart. The fix is to wait for a running apply, then run `apply`, which settles it. |
| `receipt` | The newest readable receipt | OK `D_OK`, WARN `D_LAST_RUN` when its outcome is not `ok`, N/A `D_NO_RECEIPT`. |
| `lock` | The lock file | OK `D_OK` (no file, or an empty file with no note) or OK `D_LOCK_NOTE`. WARN `E_STORAGE_INVALID` when the file is a link, a directory, a FIFO, owned by another user, or cannot be opened or read. The file is opened without following a link and checked on the descriptor. |
| `profile` | The resolved profile | OK `D_OK`. ERROR with the resolver's code (`E_MANIFEST_INVALID`, `E_SOURCE_MISSING`, `E_PROFILE_INVALID`, and so on). UNKNOWN `D_DEPENDS_UNKNOWN` without a readable binding. |
| `module` | Each selected module | OK `D_OK`. |
| `resource` | A resource that is not a file output (package, application and similar) | UNKNOWN `D_PROBING_NOT_BUILT`, not required. N/A `D_NA_SENSITIVE` when it is sensitive. |
| `plan` | The plan could not be made | ERROR with the planner's code. UNKNOWN `E_OBSERVATION_UNKNOWN`. UNKNOWN `D_DEPENDS_UNKNOWN` after a profile failure. |
| `output` | A planner blocker | ERROR with the blocker's code (`E_CONFLICT`, `E_PARENT_UNSAFE`, `E_TARGET_RESERVED`, and so on). The blocker's correction is the fix. |
| `output` | An owned output (ledger) or a claimed output (plan) | See the next table. |

### Output rows

An output row joins three facts: the ledger entry (who owns it), the live entry (present, fingerprint) and the plan operation (would the plan change it). A path that no ledger entry and no plan operation names has no row, so a project-local override or any other foreign file is never a finding.

| Situation | Status and code |
| --- | --- |
| Owned, present, fingerprint matches, plan changes nothing | OK `D_OK` |
| Owned, present, matches, plan would update it (the wanted result changed) | WARN `D_PLAN_PENDING` |
| Owned, matches, nothing selects it any more | WARN `D_PLAN_REMOVAL` |
| Owned and gone | WARN `D_OUTPUT_MISSING` |
| Owned, edited, replaced or retargeted | ERROR `E_CONFLICT` |
| Owned, a parent is a link or not a directory | ERROR `E_PARENT_UNSAFE` (found from the ledger when no plan exists) |
| Owned, live entry cannot be read | UNKNOWN `E_OBSERVATION_UNKNOWN` |
| Claimed by the plan, not owned, plan would create it | WARN `D_PLAN_PENDING` |
| Claimed, not owned, already the wanted result | WARN `D_UNRECORDED` |
| Sensitive output: a ledger entry of the sensitive class (compared with case folded), a path a selected sensitive resource claims, or a sensitive plan operation | N/A `D_NA_SENSITIVE`. No content is read or hashed, whoever owns the path. A planner blocker for the path (for example `E_CONFLICT` for a sensitive destination without a ledger entry) is shown instead, because it needs no content. |
| Operation that needs generated content | N/A `D_NA_GENERATED`: not applicable in this build (no generator). |

An owned output is compared by fingerprint: SHA-256 of the file content, or of the link target text for a link (`link_fingerprint`). The comparison reads the file (a read, never a write).

## Codes

`D_*` codes belong to `doctor`. They are registered in one place, `dfm_model::doctor::CODES`, and a test requires each one to appear in this file.

| Code | Meaning |
| --- | --- |
| `D_OK` | The check passed. |
| `D_NA_SENSITIVE` | A sensitive output. Its content is never read or hashed. |
| `D_NA_GENERATED` | A generated file. No generator exists in this build. |
| `D_OUTPUT_MISSING` | An owned output no longer exists. Apply publishes it again. |
| `D_PLAN_PENDING` | The plan would create or update this output. Nothing owned was edited. |
| `D_PLAN_REMOVAL` | The plan would remove this owned output. Nothing selects it any more. |
| `D_UNRECORDED` | The output already is the wanted result, but the ledger does not record it. |
| `D_STATE_PARTIAL` | A state file that is only audit evidence is unreadable or stray. The inventory is partial. |
| `D_LAST_RUN` | The last recorded run did not end with `ok`. |
| `D_NO_RECEIPT` | No run has been recorded yet. |
| `D_STATE_ABSENT` | No binding and no state exist yet. |
| `D_LOCK_NOTE` | The lock file holds a note from an earlier holder. |
| `D_DEPENDS_UNKNOWN` | A check cannot run because an earlier check failed. |
| `D_PROBING_NOT_BUILT` | Tools, versions and PATH are not probed in this build. |

`E_*` codes keep the meaning given in [architecture.md](architecture.md), [state.md](state.md), [resolver.md](resolver.md) and [planner.md](planner.md).

## Independent rows

Each source is read on its own. A parse failure in the ledger, the journal, a module manifest or the profile gives one ERROR row for that source and leaves the other rows in place.

- The ledger, journal and state document are read with the lenient reads. One output with an unsafe parent does not hide the rest of the ledger.
- When the profile or the plan fails, owned outputs still get rows from the ledger and the live entry. They are labeled `partial`, because drift against the plan was not assessed.
- When the ledger fails, the plan cannot know ownership, so the plan row fails and output rows for the plan are absent.
- A ledger path outside HOME is refused when the ledger is read, so it fails the ledger row. The per-path ERROR row for such a path is a second line of defense.
- The plan's observations serve the output rows. A second read-only pass covers only owned paths the plan did not observe or hash.

## Table format

Each row starts a line with `STATUS CODE kind`. Indented lines follow: `subject:`, and for `doctor` also `detail:` and `fix:`. Text wraps at spaces, and a word longer than a line breaks inside the word. No line is longer than the width. The width is 80 columns, with a minimum of 40. A `test-isolation` build reads `DFM_WIDTH`. The last lines give the result and the counts. Full paths are in `--json`, where nothing wraps.

## Not probed in this build

These are deferred. A row for them is UNKNOWN with the reason "probing not built" and is not required, or it is absent.

- Tool presence, tool versions and the wanted, installed, selected and running versions.
- PATH context, shims and link resolution outside the owned outputs.
- Remote eligibility and update freshness.
- Active trials and interfering unmanaged installations.
- Whether the process that wrote a lock note still runs. Doctor does not take the lock.

## Known limits

- The table width is fixed at 80 columns outside `test-isolation`. No terminal size is read.
- A parent of an owned output that is a link is reported as an error even when the link stays inside HOME, because apply refuses to write through it.
- The planner reports a link that leaves HOME as `E_TARGET_RESERVED`. Doctor shows the planner's code.
- A receipt that fails the claim check (for example its output now sits below a link) is shown as an unreadable receipt, a WARN row.
- Doctor reads file content to compare fingerprints. It reads nothing for a sensitive output. The rule is the planner's (`dfm_model::plan::sensitive_paths`), with the claims of selected sensitive resources compared case-folded. Without a resolved profile only the ledger classes protect a path, so a normal ledger entry that a sensitive resource now claims is hashed in that case.
- `plan_for` still opens the store and reads the ledger on its own, so doctor reads the ledger twice.
