# Apply (DFM-18)

Task: DFM-18. This document records what the first `apply` does and where it resolves or differs from [docs/architecture.md](architecture.md) (the contract). The contract wins where this document is silent. The planner is in [docs/planner.md](planner.md) and the ledger in [docs/state.md](state.md).

## What exists

| Part | Location |
| --- | --- |
| Descriptor-relative primitives: open a verified parent, `mkdirat`, `symlinkat`, staged publish, replace, `unlinkat` | `crates/engine/src/publish.rs` |
| Execution: gates, rechecks, journal, ledger, accounting | `crates/engine/src/apply.rs` |
| Command, approval, second plan under the lock, receipt, report | `crates/cli/src/apply_cmd.rs` |
| Receipt field `reload` | `crates/model/src/state.rs` |

`model` stays pure. `engine` does all file input and output through the safe `rustix` API. `unsafe_code` stays forbidden. No dependency was added.

## The command

```
dotfiles-manager apply --root <dir> [--preview] [--yes-plan <digest>] [--allow-disruptive <operation-id>]...
```

There is no `--plan <file>` flag. The manager stores no plan (architecture section 10), so apply plans again from the current machine and compares the digest the reviewer approved.

1. Apply resolves, observes and plans exactly as `plan` does. It prints the plan and its digest.
2. `--preview` stops here and exits 0. It cannot be combined with `--yes-plan` (exit 2).
3. A blocked plan (any blocker) exits 3 with the first blocker. A blocked plan cannot be approved.
4. Without `--yes-plan`, apply exits 3 with `E_APPROVAL_REQUIRED` and changes nothing. No prompt exists, so every run is noninteractive (architecture section 10, "Bounded noninteractive behavior").
5. A `--yes-plan` digest that differs from the current digest exits 3 with `E_PLAN_STALE`. This check is read-only. It does not take the lock and does not create the lock file.
6. Apply takes the writer lock, plans a second time and compares the digest again. A change between the first look and the lock is also `E_PLAN_STALE`.
7. Apply runs the pre-mutation gates (below), settles interrupted work, executes, writes a receipt and prints the report.

The approval kind in the receipt is `yes_plan`. The engine cannot check that a person read the plan (architecture section 14, recorded limit).

## Gates before the first write

All of these exit 3 and change nothing:

- The plan has a blocker (`E_CONFLICT`, `E_PARENT_UNSAFE`, `E_PRIVILEGE_UNSUPPORTED`, and the other planner codes).
- The digest is stale or missing.
- Each operation is checked for shape: its `after` entries come earlier in the order, its journal id is valid, a link target lies inside the source root, and its path has no empty, `.` or `..` component.
- A planned file operation needs content. No adapter exists, so no generator exists in the release binary (`E_GENERATOR_UNAVAILABLE`). A plan with a generated file is refused as a whole, so apply never publishes half of a profile. A sensitive file output is also refused (`E_GENERATOR_UNAVAILABLE`), because conflict detection for sensitive paths does not exist (docs/state.md).
- Each planned `remove` operation needs its own `--allow-disruptive <operation-id>` (`E_DISRUPTIVE_UNAPPROVED`). Architecture section 10 item 8 lists deletion of an owned file as disruptive. An allowance for an id that is not a planned removal is a usage error (exit 2).

## Order

Operations run in the planner's order. It is a deterministic topological sort of the `after` edges: directories first, then links and files, with removals ordered by the planner's edges. Apply never reorders.

For one operation, apply does these steps in this order:

1. **Prerequisites.** Apply observes the path and its parents again, through the opened HOME, with the planner's own request and string builders (`observe`, `dfm_model::plan::node_state`, `ledger_state`). Every `path:` and `ledger:` precondition must equal the fresh string. A mismatch makes the operation `blocked` before anything is written. No journal entry exists for it.
2. **Topology.** Apply opens the parent chain from HOME one component at a time with `O_NOFOLLOW | O_DIRECTORY`. Each directory must be owned by the current user. A link, a file, a missing directory or another owner is `E_PARENT_UNSAFE`. All later steps use the opened parent descriptor, never the path.
3. **Leaf.** Apply looks at the leaf through that descriptor (`fstatat` without following). It must be what the operation expects: absent for a create, the same link for a replace, a regular file for an update, a file or link for a remove.
4. **Content.** For a file, the generator produces the bytes. A generator failure leaves no journal entry.
5. **Intent.** The journal gets an `intent` entry (durable) with the path, whether it existed, and the expected SHA-256.
6. **Effect.** See the next section.
7. **Read-back.** Apply reads the result through the same descriptor: link target, content hash and mode, or absence.
8. **Outcome.** The journal gets `done`. If the effect or the read-back failed, it gets `failed` with a redacted reason.
9. **Ownership.** The ledger is rewritten atomically with the new output (or without the removed one). Ownership is recorded only after `done`.

The receipt is written once, after the last operation (architecture section 9 item 4).

## Effects and what each one verifies

| Action | Primitive | Last check |
| --- | --- | --- |
| `create_dir` | `mkdirat`, mode 0700 | `EEXIST` is a conflict. The mode is stated, not inherited from the umask. A created directory is never recorded as owned and never removed. |
| `create_link` | `symlinkat` | `EEXIST` is a conflict. A leaf that appeared is never replaced. |
| `replace_link` | `symlinkat` to a hidden name, then `renameat` | The live target must equal the target the ledger vouches for, read just before the rename. |
| `create_file` | staged file (`O_EXCL`, mode set before content, `fsync`), then `linkat` | `linkat` never replaces. The staged name is removed. The directory is synced. |
| `update_file` | staged file, then `renameat` | The live content hash must equal the ledger fingerprint, read just before the rename. |
| `remove` | `unlinkat` without `AT_REMOVEDIR` | See Delete safety. |

Staged names are `.<name>.<pid>.<n>.<nanos>.stage`. A failure before the commit point removes the staged file and leaves the previous state. A failure after it is `E_IO_UNCONFIRMED`.

Apply never uses a `force` relink, never replaces a directory, never writes through a parent it did not open without following links, and never deletes recursively. `Dir::remove_entry` refuses a directory.

Directory links exist only as explicit claims. The planner makes one operation per claim. A claim below an existing link parent is `E_PARENT_UNSAFE` at plan time.

## Delete safety

A removal needs all of the following at the moment of deletion:

1. The plan lists it as `remove` (the planner proved the ledger fingerprint matches the live output and nothing selects the path).
2. `--allow-disruptive <operation-id>` names it.
3. The recorded preconditions still hold (the file or link is unchanged since planning).
4. `Store::authorizes_delete` returns true now: the ledger is read again, the path is inside HOME and outside the manager's paths (by text and by filesystem identity), the resource is `normal` class, and the fingerprint recorded equals the live one. The live fingerprint comes from a descriptor opened with `O_NOFOLLOW` (file content hash) or from `readlinkat` (link target text hash).
5. The entry is not a directory, and is not sensitive.
6. The name still refers to the entry that was hashed (device and inode for a file, the same target for a link).

Then `unlinkat` removes the name. A link is removed, never followed. Anything modified or foreign is kept and reported. Deselecting a module removes only the outputs the ledger proves the manager wrote. It does not uninstall a package, erase runtime state, remove a directory, or touch a sibling.

A removal candidate that is already gone has a stale ledger entry. Apply drops that entry (journaled), so a later file with the same content at the same path does not look owned.

## Journal ids

The planner's `journal_id` ignores observations. A rerun after a partial apply plans the same id. Apply forms the id it writes as `<journal_id>-<n>`, where `n` is one more than the number of `intent` entries already in the journal whose id starts with `<journal_id>-`. The lock is held, so the count cannot race. Every attempt has a distinct id and an interrupted attempt keeps its record. The receipt uses the attempt id for an operation that ran, and the planner id for one that did not.

## Interrupted runs

An `intent` entry without a terminal entry is an interruption record. At the start of a run, after the gates, apply settles each one (`Store::reconcile`) with the operation's `already_done` check: the live entry is read through the opened HOME and compared with the recorded expectation. The result is journaled `done`, `failed` or `unknown`. Apply runs nothing again and creates no ownership. An `unknown` result blocks every operation on that path. Settling writes the journal, so it counts as a mutation.

## Failure and accounting

- The first operation that fails or blocks stops all further mutation. This is the default and there is no `--continue`.
- An operation that depends on it (through `after`, transitively through results) is `blocked`. Every other operation that has not started is `not_started`. `current` and `not_in_scope` operations are reported as they are.
- Completed operations stay `applied`. There is no rollback.
- The report lists one line per operation and the counts `applied`, `current`, `failed`, `blocked`, `not_started`, `not_in_scope`. The receipt has the same counts under the contract status names (`updated` is printed as `applied`).

| Result | Exit |
| --- | --- |
| Everything applied, current or not in scope | 0 |
| A failure or block, and something was already written (any journal entry or effect) | 1 (`E_APPLY_FAILED`) |
| A block before anything was written (stale recheck on the first operation, topology refusal) | 3, with the code of the first problem |
| Gates (above) | 3 |
| Usage | 2 |

A receipt is written whenever the run wrote anything, also after a failure. Its `result.outcome` and `exit_code` follow architecture section 5. A run that wrote nothing writes no receipt. A safe rerun of an applied plan is exactly that: every operation is `current`, no journal entry, no ledger write, no receipt, exit 0. It takes and releases the lock, which rewrites the lock holder note.

## Publication and reload

Publishing a file does not reload the program that reads it (architecture section 8, "A correct file, an installed executable and a reloaded consumer are three separate postconditions"). Each published link or file has `reload` in its receipt operation. The only value apply writes is `not_attempted`, because no `Reloader` exists. The validator accepts the contract names (`requested`, `reloaded`, `unsupported`, `failed`) for later tasks. The report prints a line that says no reload happened. A test asserts that no `updated` operation claims a reload.

## Decisions where the documents were silent

| Question | Choice |
| --- | --- |
| Flag shape | `--root`, `--yes-plan`, `--allow-disruptive` (repeatable), `--preview`. No `--plan <file>`: the digest is the contract. |
| Is a removal approved by `--yes-plan` alone? | No. Architecture section 10 item 8 calls deletion of an owned file disruptive, so each removal needs `--allow-disruptive <operation-id>`. This also bounds mass deletion: N removals need N explicit ids. The planner still has no mass-removal guard (docs/planner.md). |
| Mode of a directory apply creates | 0700, the XDG default. The planner has no mode for `mkdir`. |
| File mode | The declared mode, else 0600. |
| An output that already is correct but unrecorded (`unrecorded_current`) | Left as it is and not claimed. Adopting an existing output is a separate workflow. |
| Directories | Never recorded as owned, never removed. |
| Receipt `sources` | Empty. A path source has no commit, and a made-up one would be false. The plan digest covers the source tree digest. |
| Receipt `approval.kind` | `yes_plan`. |
| New codes | `E_APPROVAL_REQUIRED` (3), `E_GENERATOR_UNAVAILABLE` (3), `E_DISRUPTIVE_UNAPPROVED` (3), `E_APPLY_FAILED` (1). |
| New receipt field | `operations[].reload`, optional, from a fixed list. The contract's receipt shape does not have it. Nothing released wrote receipts before, so no schema bump is made. |

## Known limits

- **No generated files from the command.** `render`, `merge` and `assemble` resources need adapters (DFM-20). The engine publishes and removes files and the tests prove it with a test generator, but the release binary has none, so `apply` refuses any plan that contains a generated file. The example personal profile is such a plan. Generated-file publication is therefore not proven through the CLI.
- **Replace and remove check, then act.** `replace_link`, `update_file` and `remove` verify the leaf and then call `renameat` or `unlinkat`. These are two system calls. A process of the same user that swaps the leaf between them wins. Create operations have no such window (`EEXIST`). A swap of a parent directory cannot redirect any step, because every step uses the opened descriptor. A parent swapped after it was opened is not detected for the operations that already hold it. The effect lands in the directory that was verified, and the next operation sees the changed topology and is blocked.
- **Source content during the run.** The `source:` preconditions are checked when the plan is made and again under the lock. They are not read again per operation. A link does not read its source. A generator must re-verify the sources it reads.
- **Crash between the effect and the ledger.** The output exists, the journal says `done` (or `intent` only), and the ledger has no entry. The next plan sees `unrecorded_current`. The output stays and is not owned, so it is never removed. This is the safe direction. Recovering ownership from the journal is not built.
- **Signals.** SIGINT and SIGTERM are not handled (no signal API without `unsafe` or a new dependency). A signal ends the process. The `intent` entry is the interruption record and the next run settles it. Exit 130 and 143 are not produced.
- **No interactive approval.** There is no prompt, so exit 4 does not occur. There is no `--timeout` or run limit.
- **Receipt write after the work.** If the receipt cannot be written, the run exits 1 and the journal and ledger are the record.
- **Case and Unicode aliases** are handled by the planner (docs/planner.md). Apply trusts the plan's alias check and the byte-exact precondition strings.
- **Privilege.** Apply never escalates. A path that needs another owner is a planner blocker.
- **Evidence scope.** Tests use the `test-isolation` build and temporary directories. No device was used.
