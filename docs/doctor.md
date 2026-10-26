# Doctor and status

`doctor` and `status` in this build. [docs/architecture.md](architecture.md) is the contract and wins where this document is silent.

| Part | Location |
| --- | --- |
| Row type, code registry, exit rule, table and JSON output | `crates/model/src/doctor.rs` |
| The shared inspection and the two commands | `crates/cli/src/doctor_cmd.rs` |
| Plan building from an already resolved profile (`plan_for`) | `crates/cli/src/plan_cmd.rs` |

## The commands

```
dfm doctor [--root <dir>] [--json]
dfm status [--root <dir>] [--json]
```

Both run one inspection and show the same rows. `doctor` prints `detail` and `fix` for each row. `status` prints the status line and the subject only. `--json` prints one document (`schema: 1`) with the same rows for both commands. `--root` defaults to the root the binding records (docs/bootstrap.md).

Both commands are read-only. They run no program and probe no tool, version or PATH entry. They fetch nothing, authenticate nothing, reload nothing, write nothing and take no lock. State is opened with the read-only store open, so the lock file is never created.

## Exit codes

| Exit | Meaning |
| --- | --- |
| 0 | No row is ERROR and no required row is UNKNOWN. A WARN row does not change the exit. |
| 1 | Any ERROR row, or any UNKNOWN row of a required check. Standard error shows `E_DIAGNOSED`. |
| 2 | Usage error (no `--root` with a schema 1 binding, unknown flag, repeated flag, `--against`), or no report can be made at all. Arguments are parsed before HOME is resolved. |

`doctor` never exits 3 or 4 (architecture section 10). A malformed manifest, a missing profile, an unreadable state file or an unusable HOME is a row and exit 1. An unusable HOME gives an ERROR `binding` row and UNKNOWN `state` and `profile` rows.

## Row fields

| Field | Meaning |
| --- | --- |
| `kind` | `binding`, `state`, `lock`, `profile`, `module`, `resource`, `output`, `intent`, `receipt` or `plan`. |
| `code` | A stable code: an existing `E_*` code, or a `D_*` code from the table below. |
| `status` | `OK`, `WARN`, `ERROR`, `UNKNOWN` or `N/A`. |
| `subject` | The path, file or resource. Output paths and stray state entries are relative to HOME or to the state directory. The `binding`, `profile` and `lock` subjects are absolute paths. A subject is cut at 4096 visible characters, after control and invisible characters became escapes, and never inside one escape. |
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
| `state` | A receipt or trial record that cannot be read, or belongs to another identity | WARN with the file's code, labeled partial. Receipts and trial records are audit evidence, never authority. A readable trial record gets no row. |
| `state` | Stray entries (interrupted writes, staging directories) | WARN `D_STATE_PARTIAL`, labeled partial. |
| `intent` | A journal intent with no outcome | ERROR `E_OP_INTERRUPTED`. A running apply also leaves an intent until it ends, and doctor cannot tell the two apart. Fix: wait for a running apply, then run `apply`, which settles it. |
| `intent` | A journal `unknown` entry that no later entry settled | ERROR `E_OP_INTERRUPTED`, required. The detail names the operation and its journal id, and says whether it blocks one path or, with no intent record, a whole resource. Fix: `state recover` or `apply`, which probe it again, or `state resolve '<journal-id>' --as done|failed` (`done` is refused for an entry with more than one path). Doctor never settles. The row stays until a later entry settles the operation. |
| `receipt` | The newest readable receipt | OK `D_OK`, WARN `D_LAST_RUN` when its outcome is not `ok`, N/A `D_NO_RECEIPT`. |
| `lock` | The lock file | OK `D_OK` (no file, or an empty file with no note) or OK `D_LOCK_NOTE`. WARN `E_STORAGE_INVALID` when the file is a link, a directory or a FIFO, is owned by another user, has a mode other than 0600, has more than one hard link, or cannot be opened or read. The writer lock opens the file read-write, so it cannot use 0400, 0200 or 0000, and it refuses group or other bits. The file is opened without following a link and checked on the descriptor. |
| `profile` | The resolved profile | OK `D_OK`. ERROR with the resolver's code (`E_MANIFEST_INVALID`, `E_SOURCE_MISSING`, `E_PROFILE_INVALID`, and so on). UNKNOWN `D_DEPENDS_UNKNOWN` without a readable binding. |
| `module` | Each selected module | OK `D_OK`. |
| `resource` | A resource that is not a file output (package, service, application and similar) | UNKNOWN `D_PROBING_NOT_BUILT`, not required. N/A `D_NA_SENSITIVE` when it is sensitive. |
| `plan` | The plan could not be made | ERROR with the planner's code. UNKNOWN `E_OBSERVATION_UNKNOWN`. UNKNOWN `D_DEPENDS_UNKNOWN` after a profile failure. |
| `output` | A planner blocker | ERROR with the blocker's code (`E_CONFLICT`, `E_PARENT_UNSAFE`, `E_TARGET_RESERVED`, and so on). The blocker's correction is the fix. |
| `output` | A name beside an owned output or an open-intent path with the older staged-copy shape `.<name>.<digits>.<digits>.<digits>.tmp` or `.stage` | WARN `D_STAGED_COPY`. Fix: "A staged copy from an interrupted run; check it, then remove it by hand." Doctor lists each directory once and reads no content. A directory with more than 100000 entries (names that are not UTF-8 count too) is read only in part, and doctor adds one partial WARN `D_STAGED_COPY` row for it. Nothing sweeps such a name, because a user file can have the same shape. |
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
| Claimed, not owned, already the wanted result | WARN `D_PLAN_PENDING` when the plan adopts it (an exact link that no ledger entry mentions; the fix names `--allow-adopt`), else WARN `D_UNRECORDED` |
| Sensitive output: a ledger entry of the sensitive class (compared with case folded), a path a selected sensitive resource claims, or a sensitive plan operation | N/A `D_NA_SENSITIVE`. A planner blocker for the path is shown instead, because it needs no content. For a `claude-settings/v1` merge output, see the next paragraph. |

An owned output is compared by fingerprint: `file_fingerprint` of the file content (SHA-256 of `file\0` and the bytes), or `link_fingerprint` of the target text for a link. The comparison reads the file and never writes it.

Doctor's own inspection reads and hashes no content of a sensitive output, whoever owns the path. The plan it embeds is the one exception: for a `claude-settings/v1` merge output, the plan reads the live file in memory and compares the managed fields by salted hash (docs/adapters.md). A healthy merge output is N/A. A lost or unusable salt is the plan's `E_STATE_INVALID` or `E_STORAGE_INVALID` blocker, and `state check` reports the salt file itself (docs/state.md). A foreign edit of a managed field, a malformed file, or a file open to group or others is ERROR with the plan's `E_CONFLICT` and fix text. Doctor prints no content, hash or value for it.

## Codes

`D_*` codes belong to `doctor`. They are registered in one place, `DoctorCode::ALL` (`dfm_model::doctor`). A test requires each code and its description to appear in this table.

| Code | Meaning |
| --- | --- |
| `D_OK` | The check passed. |
| `D_NA_SENSITIVE` | A sensitive output. Its content is never read or hashed. |
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
| `D_STAGED_COPY` | A name beside an owned output has the shape of a staged copy of an older build. Nothing removes it. |

`E_*` codes keep the meaning given in [architecture.md](architecture.md), [state.md](state.md), [resolver.md](resolver.md) and [planner.md](planner.md).

## Independent rows

Each source is read on its own. A parse failure in the ledger, the journal, a module manifest or the profile gives one ERROR row for that source and leaves the other rows in place.

- The ledger, journal and state document are read with the lenient reads. One output with an unsafe parent does not hide the rest of the ledger.
- When the profile or the plan fails, owned outputs still get rows from the ledger and the live entry. They are labeled `partial`, because drift against the plan was not assessed.
- When the ledger fails, the plan cannot know ownership, so the plan row fails and output rows for the plan are absent.
- A ledger path outside HOME is refused when the ledger is read, so it fails the ledger row. The per-path ERROR row for such a path is a second line of defense.
- The plan's observations serve the output rows. A second read-only pass covers only owned paths the plan did not observe or hash.

## Table format

Each row starts a line with `STATUS CODE kind`. Indented lines follow: `subject:`, and for `doctor` also `detail:` and `fix:`. Text wraps at spaces, and a word longer than a line breaks inside the word. No line is longer than the width. The width is fixed at 80 columns. No terminal size is read. Only a `test-isolation` build reads `DFM_WIDTH` (minimum 40). The last lines give the result and the counts. Full paths are in `--json`, where nothing wraps. The table is not colored.

## Not probed in this build

A row for these is UNKNOWN `D_PROBING_NOT_BUILT` and not required, or it is absent.

- Tool presence, tool versions and the wanted, installed, selected and running versions. `packages` and `services` probe their providers; `doctor` does not.
- PATH context, shims and link resolution outside the owned outputs.
- Remote eligibility and update freshness.
- Active trials (no per-trial row) and interfering unmanaged installations.
- Whether the process that wrote a lock note still runs. Doctor does not take the lock.

## Known limits

- A parent of an owned output that is a link is an error even when the link stays inside HOME, because apply refuses to write through it.
- The planner reports a link that leaves HOME as `E_TARGET_RESERVED`. Doctor shows the planner's code.
- A receipt that fails the claim check (for example, its output now sits below a link) is shown as an unreadable receipt, a WARN row.
- Doctor reads file content to compare fingerprints. It does not read a path that another resource's ledger entry claims. It applies the planner's exclusion (`dfm_model::plan::sensitive_paths`: sensitive ledger entries, ledger entries owned by a selected sensitive resource, and the claims of selected sensitive resources) and compares paths case-folded. Without a resolved profile, only the sensitive ledger classes protect a path. In that case, doctor hashes a normal ledger entry that a sensitive resource owns or now claims.
- `plan_for` opens the store and reads the ledger on its own, so doctor reads the ledger twice.
