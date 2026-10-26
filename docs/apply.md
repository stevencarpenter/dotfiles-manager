# Apply

This page records what `apply` does where [docs/architecture.md](architecture.md) (the contract) leaves a choice open. The contract wins where this page is silent. The plan it executes is in [docs/planner.md](planner.md) and the store in [docs/state.md](state.md). The co-owned merge rules are in [docs/adapters.md](adapters.md).

## What exists

| Part | Location |
| --- | --- |
| Descriptor-relative primitives: open a verified parent, `mkdirat`, `symlinkat`, staged publish, replace, `unlinkat` | `crates/engine/src/publish.rs` |
| Execution: gates, rechecks, journal, ledger, accounting | `crates/engine/src/apply.rs` |
| Settling interrupted work | `crates/engine/src/recover.rs` |
| Signal handler | `crates/engine/src/signal.rs` |
| Command, approval, second plan under the lock, receipt, report | `crates/cli/src/apply_cmd.rs` |

`model` stays pure. `engine` does all file input and output through the safe `rustix` API. `unsafe_code` stays forbidden everywhere except `crates/engine/src/signal.rs`, which is also the only user of the direct `libc` dependency (see Signals).

`secrets`, `packages` and `services` run this same command path with another plan scope ([docs/planner.md](planner.md), "Scope of a plan"). `sync` calls it once after it publishes the sources ([docs/sync.md](sync.md)).

## The command

```
dfm apply [--root <dir>] [--preview] [-y|--yes-plan <digest>] [--allow-disruptive <operation-id>]... [--allow-adopt <operation-id>]...
```

There is no `--plan <file>` flag. The manager stores no plan (architecture section 10), so apply plans again from the current machine and compares the digest the reviewer approved. `--root` defaults to the root the binding records.

1. Apply resolves, observes and plans exactly as `plan` does. It prints the plan and its digest.
2. `--preview` runs the gates that need no write (below), including the journal size, and exits 0 only when apply would not refuse on those gates. It lists every removal that lacks its flag as a pasteable `--allow-disruptive '<id>'` flag and exits 3 with `E_DISRUPTIVE_UNAPPROVED`, as apply does. It lists an adoption without its flag the same way, as `--allow-adopt '<id>'`, and exits 3 with `E_ADOPT_UNAPPROVED`. When both kinds are missing, both lists print and the first error is the one returned. Pass the flags to the preview to get exit 0. A preview never writes. It cannot be combined with `--yes-plan` (exit 2).
3. A blocked plan (any blocker) exits 3 with the first blocker. A blocked plan cannot be approved.
4. Without `--yes-plan` (short form `-y`), apply exits 3 with `E_APPROVAL_REQUIRED` and changes nothing. No prompt exists, so every run is noninteractive (architecture section 10, "Bounded noninteractive behavior"). Exception (owner decision, 2026-10-25): a plan that writes nothing needs no approval. Every operation is `current`, `held` or `not_in_scope`, no `current` operation is a removal (apply would prune its ledger entry), and the journal holds no unfinished entry and no unknown outcome. Apply then prints `nothing to change: no approval needed` and `result: ok`, exits 0, takes no lock and writes no receipt.
5. A `--yes-plan` digest that differs from the current digest exits 3 with `E_PLAN_STALE`. This check is read-only. It does not take the lock and does not create the lock file.
6. Apply takes the writer lock, plans a second time and compares the digest again. A change between the first look and the lock is also `E_PLAN_STALE`.
7. Apply runs the pre-mutation gates (below), settles interrupted work, executes, writes a receipt and prints the report. The last line is `result: <name>`. It is the exit name of the exit code the process returns, from the same value.

An `--allow-disruptive` id that names no removal in the first plan, and an `--allow-adopt` id that names no adoption, is a usage error (exit 2). This check runs before the lock is taken and before the state store is opened for writing.

The approval kind in the receipt is `yes_plan`. The engine cannot check that a person read the plan (architecture section 14, recorded limit).

## Gates before the first write

All of these exit 3 and change nothing:

- The plan has a blocker (`E_CONFLICT`, `E_PARENT_UNSAFE`, `E_PRIVILEGE_UNSUPPORTED`, and the other planner codes).
- The digest is stale or missing.
- Each operation is checked for shape: its `after` entries come earlier in the order, its journal id is valid, a link target lies inside the source root, and its path has no empty, `.` or `..` component. An operation that stages a file or link under a hidden name (`create_file`, `update_file`, `replace_link`, `retire`) also needs a leaf name of at most 216 bytes, because the staged name adds up to 39 bytes and a name holds 255 (`E_LIMIT_EXCEEDED`). A leaf over 255 bytes is refused the same way for every other action.
- A planned file operation needs content. The generator must support it and the plan must carry its output hash (`output_sha256`). Otherwise the gate refuses with `E_GENERATOR_UNAVAILABLE` or `E_PLAN_INVALID`. This applies to the whole plan, so apply never publishes half of a profile. The release binary has generators for `git-config/v1` and `text-fragments/v1`. `claude-settings/v1` takes its own path (see Merge).
- Any planned operation on a sensitive output is refused with `E_SENSITIVE_UNSUPPORTED`, unless the generator runs it as a merge (`Generator::merges`) or renders it as a secret (`Generator::renders`, [docs/secrets.md](secrets.md)). Today that is exactly the `create_file`, `update_file` and `retire` operations of `claude-settings/v1`, and the `create_file` and `update_file` operations of `op-template/v1` in a `secrets` run. A sensitive link, a sensitive directory and a sensitive removal stay refused, so the executor never meets one. One refused operation refuses the whole plan. A merge operation must carry `managed_sha256` (`retire` must carry none), otherwise the plan is `E_PLAN_INVALID`.
- A link target must lie in the root of the operation's own source: the plan source whose `<root>/<first input path>` is the target. A target outside every source root, or one that names another file than the operation's input, is `E_PLAN_INVALID`.
- The journal must be able to take this run's entries. The check assumes 4096 bytes for the two entries of each journaled operation. It also reserves 4096 bytes for each entry that settling earlier work will append: one per open intent, one per open `unknown` entry that has an intent record, and one for the repair of a torn final line. An `unknown` entry with no intent record is never closed and reserves nothing. If the journal would pass 8 MiB, apply refuses with `E_LIMIT_EXCEEDED`. Journal rotation is not built ([docs/state.md](state.md), "Recovery by hand").
- Each planned `remove` operation (and each service `restart`, [docs/lifecycle.md](lifecycle.md)) needs its own `--allow-disruptive <operation-id>` (`E_DISRUPTIVE_UNAPPROVED`). The allow set must equal the set of planned removals, so the plan determines it. The refusal lists every missing id as a pasteable flag, with no cap. Architecture section 10 item 8 lists deletion of an owned file as disruptive, so `--yes-plan` alone never approves one, and N removals need N explicit ids.
- Each planned `adopt` operation, and each merge `takeover`, needs its own `--allow-adopt <operation-id>` (`E_ADOPT_UNAPPROVED`). The allow set must equal the set of planned adoptions, as for removals. An adoption changes what the manager may later delete, so `--yes-plan` alone does not approve it. The two allow sets are separate, so one kind of approval never satisfies the other. A link target of an adoption is checked against its source root like any other link.

## Generator

`Generator` is a trait in `dfm_engine::generate`. The adapters implement it in the CLI crate (`crates/cli/src/adapters/generate.rs`). `generate(&Operation, &GeneratorInputs) -> Result<Vec<u8>, Refusal>` never opens a file. Apply collects the inputs first. (A merge does not use this path: see Merge.)

1. **Sources.** Each `Operation.inputs` path is read below the plan's source root. Every parent and the file are opened with `O_NOFOLLOW`, the file must be regular, owned by the current user, with one hard link, and at most 16 MiB (`MAX_HASHED_BYTES`). The hash must equal the operation's `source:` precondition. A plan with generated files must have exactly one source.
2. **Fragment directory.** For an `assemble` operation, apply opens `Operation.fragment_dir` from HOME one component at a time without following a link, lists it (names only) and reads each fragment the plan lists through that descriptor.
   - A foreign fragment must have the `file_fingerprint` recorded in `Fragment.sha256`, which the planner observed.
   - A fragment from a selected generated producer must hash to that producer's `output_sha256`.
   - A fragment from a selected link producer must be the link the plan made. Its content is the producer's source file, read and compared like any other source.
   - Together the fragments are at most 16 MiB (`E_LIMIT_EXCEEDED`), checked before each read. The listing is cut at 50,000 entries and a cut listing is stale.
   - Every input file is opened without following a link and must be a regular file owned by the current user with one hard link. A fragment that is gone, a link, a FIFO, hard-linked, over 16 MiB or different is stale. The message names the path and does not print a hash. A directory that does not exist holds no fragments, and is stale only if the plan lists one.
3. **Generate.** The generator gets the bytes and the live listing. It returns the content or a refusal. A refusal with `E_PLAN_STALE` (for example a fragment that appeared) blocks the operation. Any other refusal fails it.
4. **Output hash.** SHA-256 of the generated bytes must equal `Operation.output_sha256`.

A stale input or output hash blocks the operation with `E_PLAN_STALE`. A generator failure fails it. Both happen before the intent entry, so nothing is journaled and nothing is written. The previous output stays in place (the last good output), the ledger still names its hash, and dependents are `blocked`, as for any other failure.

## Link source

A link publishes a target that the plan hashed (`source:<path>` precondition). The checkout can change after the locked re-plan, so apply reads the source again immediately before the intent entry:

1. The plan's source root (the source whose root holds the link target) is opened. Each component of `Operation.inputs[0].path` is opened below it with `O_NOFOLLOW`. A missing parent, or a parent that is a link, is stale.
2. The last component must be a regular file or a directory. Anything else, or a missing source, is stale.
3. A file must hash to the `source:` precondition. A directory is walked without following links, and its tree digest (files with their hashes, links with their target text, directories including empty ones) must equal the precondition. The walk uses the loader's limits (`MAX_SOURCE_ENTRIES` 20,000, `MAX_SOURCE_FILE_BYTES` 1 MiB, `MAX_SOURCE_TOTAL_BYTES` 64 MiB, shared constants in `dfm_model::plan`) and has no depth limit, as the loader has none. A tree over the limits is stale. An error while opening the source root or examining an entry is a failure of the operation (`E_IO_FAILED` or the storage code), not a stale source.
4. A mismatch is `E_PLAN_STALE`: the operation is blocked, nothing is journaled, nothing is written, no ownership is recorded, and the message names the path without a hash.

A source with no recorded identity (a secret value) is checked for presence and type only. A directory source that holds entries left out by `[sources].exclude` is not compared with a tree digest, because the plan records the number of excluded entries and not the patterns. Apply walks it and checks every link in it instead ([docs/resolver.md](resolver.md), "Source roots and excludes", item 6). Unlike a generated file's input, a link source is not required to be owned by the current user or to have one hard link, because the loader that hashed it did not require that either.

## Order

Operations run in the planner's order. It is a deterministic topological sort of the `after` edges: directories first, then links and files, with removals ordered by the planner's edges. Apply never reorders.

For one operation, apply does these steps in this order:

1. **Prerequisites.** Apply observes the path and its parents again, through the opened HOME, with the planner's own request and string builders (`observe`, `dfm_model::plan::node_state`, `ledger_state`). Every `path:` and `ledger:` precondition must equal the fresh string. A mismatch makes the operation `blocked` before anything is written. No journal entry exists for it.
2. **Topology.** Apply opens the parent chain from HOME one component at a time with `O_NOFOLLOW | O_DIRECTORY`. Each directory must be owned by the current user. A link, a file, a missing directory or another owner is `E_PARENT_UNSAFE`. All later steps use the opened parent descriptor, never the path.
3. **Leaf.** Apply looks at the leaf through that descriptor (`fstatat` without following). It must be what the operation expects: absent for a create, the same link for a replace, a regular file for an update, a file or link for a remove. For a replace, update, remove or adopt, the leaf must also be owned by the current user: another owner is `E_PRIVILEGE_UNSUPPORTED`, blocked, with no journal entry. The prerequisites compare no owner, so this check catches a leaf swapped in after the plan. The owner is checked again where the leaf is replaced or removed: `replace_link` and a file replace check it beside their own link-text or content check, just before the rename, and a remove checks it just before the unlink (`Dir::remove_owned`). A file replace also compares the identity of the inode it hashed with the inode the owner check examined, and a different inode is `E_CONFLICT`. The adoption read-back requires the link to be owned by the current user as well as to have the planned text. A leaf of another owner at that point is `E_CONFLICT` and is left alone. These checks narrow the check-then-act window and do not close it (see Known limits).
4. **Content.** For a file, apply reads the operation's inputs and calls the generator (see Generator). For a link (`create_link`, `replace_link`, `adopt`), apply reads the source again (see Link source). A stale input, a changed source, a generator failure or a hash that differs from the plan leaves no journal entry and no write.
5. **Intent.** The journal gets an `intent` entry (durable) with the path, whether it existed, and the expected fingerprint (`expected_sha256`, a `file_fingerprint` for a file).
6. **Effect.** See the next section.
7. **Read-back.** Apply reads the result through the same descriptor: link target, content hash and mode, or absence.
8. **Outcome.** The journal gets `done`. If the effect or the read-back failed, it gets `failed` with a redacted reason.
9. **Ownership.** The ledger is rewritten atomically with the new output (or without the removed one). Ownership of a new output is recorded after `done`. For `update_file` and `replace_link` the output is already owned, so the ledger moves to the new fingerprint before `done`. A crash or a failed ledger write then leaves an open intent, and `state recover` records the fingerprint that is live (see Interrupted runs). `apply` plans before it settles, so until then the plan reports the output as changed (`E_CONFLICT`) and its fix text names `state recover`.

The receipt is written once, after the last operation (architecture section 9 item 4).

## Effects and what each one verifies

| Action | Primitive | Last check |
| --- | --- | --- |
| `create_dir` | `mkdirat`, then `fchmodat` to 0700 by name without following a link | `EEXIST` is a conflict. The mode is set before the directory is opened, so a restrictive umask cannot leave a directory the process cannot open. A created directory is never recorded as owned and never removed. |
| `create_link` | `symlinkat` | `EEXIST` is a conflict. A leaf that appeared is never replaced. |
| `replace_link` | `symlinkat` to a hidden name, then `renameat` | The fingerprint of the live link (`link_fingerprint` of its target text) must equal the ledger fingerprint, read just before the rename. |
| `create_file` | staged file (`O_EXCL`, mode set before content, `fsync`), then `renameat` with `RENAME_EXCL` | The exclusive rename never replaces and leaves one name for the file, so no second link exists at any time. The directory is synced. |
| `update_file` | staged file, then `renameat` | The live file fingerprint (`file_fingerprint`) must equal the ledger fingerprint, read just before the rename. |
| `remove` | `unlinkat` without `AT_REMOVEDIR` | See Delete safety. |
| `adopt` | none | No effect on HOME. See Adoption. |
| merge `create_file`, `update_file`, `retire` | staged file (`O_EXCL`, 0600, `fsync`), then an exclusive `renameat` (create) or `renameat` guarded by file identity | See Merge. |

A directory apply creates has mode 0700, the XDG default; the planner has no mode for `mkdir`. A file has the declared mode, else 0600. A link is given mode 0755 after it is made, because macOS applies the umask to a link and a link made under a restrictive umask cannot be read by its owner.

**Staged names.** During `apply` the staged name is `.<name>.<tag>.tmp` (a file) or `.<name>.<tag>.stage` (a link). `<tag>` is the first 16 hex digits of the SHA-256 of the attempt's journal id, a NUL byte and the `at` time of its intent entry. The time keeps the name unique when a journal is archived by hand and journal ids start again. Two attempts with one id in the same second still share a tag. A failure before the commit point removes the staged name and leaves the previous state.

A crash between staging and the rename leaves that name. The next `apply` or `state recover` removes exactly those two names beside every path of an open intent or `unknown` entry (a regular file or link of the current user only, never followed). Any other file is left alone, including one that only looks like a staged name and the older format `.<name>.<pid>.<n>.<nanos>.tmp`. `doctor` reports such a name beside an owned output or an open-intent path as `D_STAGED_COPY` ([docs/doctor.md](doctor.md)). Remove it by hand. A name that cannot be removed is printed as `warning: ...`. After a removal the directory is synced, as after a publish; a sync that fails is a warning too, and the removal is unconfirmed. So is a parent that cannot be opened (missing, a link, a file or another owner): the copies under it stay, and because that run still settles the intent, the warning is the only report of them.

Apply never uses a `force` relink, never replaces a directory, never writes through a parent it did not open without following links, and never deletes recursively. `Dir::remove_entry` refuses a directory. Directory links exist only as explicit claims, one operation per claim. A claim below an existing link parent is `E_PARENT_UNSAFE` at plan time.

## Delete safety

A removal needs all of the following at the moment of deletion:

1. The plan lists it as `remove` (the planner proved the ledger fingerprint matches the live output and nothing selects the path).
2. `--allow-disruptive <operation-id>` names it.
3. The recorded preconditions still hold (the file or link is unchanged since planning).
4. `Store::authorizes_delete` returns true now: the ledger is read again, the path is inside HOME and outside the manager's paths (by text and by filesystem identity), the resource is `normal` class, and the fingerprint recorded equals the live one. The live fingerprint comes from a descriptor opened with `O_NOFOLLOW` (`file_fingerprint` of the content) or from `readlinkat` (`link_fingerprint` of the target text).
5. The entry is not a directory, and is not sensitive.
6. The name still refers to the entry that was hashed (device and inode for a file, the same target for a link).

Then `unlinkat` removes the name. A link is removed, never followed. An immutable flag (`uchg`) or a sticky directory owned by another user makes `unlinkat` fail with `EPERM`. That is reported as `E_IO_FAILED` with the cause, and the entry is kept. Anything modified or foreign is kept and reported. Deselecting a module removes only the outputs the ledger proves the manager wrote. It does not uninstall a package, erase runtime state, remove a directory, or touch a sibling.

A removal candidate that is already gone has a stale ledger entry. Apply drops that entry (journaled, `prune`), so a later file with the same content at the same path does not look owned. This includes the entry of a co-owned file whose file is gone. Apply checks absence through the opened HOME before the intent entry and again right before the ledger write. A path that exists again, a parent that cannot be opened, or a path in an unknown state keeps its entry. An unconfirmed ledger write there is `unknown`, like any other. A signal seen before the intent entry of the drop keeps the entry and writes nothing.

## Adoption

An `adopt` operation records ownership of a link that already is the link the plan would publish: a stow or chezmoi style setup, or an output whose ledger write was interrupted ([docs/state.md](state.md), rule 5). The planner decides when it is offered ([docs/planner.md](planner.md), rule 5). Apply writes nothing to HOME for it: no write, chmod, rename or link call. Order:

1. Prerequisites as for any operation: `path:` must equal the link state and `ledger:` must still be `none`, else the operation is `blocked` with `E_PLAN_STALE`.
2. Topology: the parent chain is opened without following a link.
3. Leaf: the entry must be a link whose target text equals `Operation.link`, else `E_CONFLICT` and the entry is left alone.
4. Intent: the journal gets an `intent` entry with `existed_before = true` and the link fingerprint as the expected value.
5. Effect step: nothing is written. Apply then reads the leaf a second time, immediately before the outcome entry (the failpoint `apply-after-effect` sits before this read). A leaf that changed gets a `failed` journal entry, the operation is `failed` with `E_CONFLICT`, and the ledger is not written.
6. Outcome: the journal gets `done`. Then the ledger records the output with the link fingerprint and mode 0777, as for a published link.

The receipt operation has status `updated`, `before` absent, `after` the link fingerprint, `owned_outputs` the path, and no `reload`, because nothing was published. The report prints the reload note only when an output was published. A rerun finds a `current` link with a ledger entry and does nothing. A later deselect removes the adopted link only with `--allow-disruptive`, like any owned link.

The window between the last read and the ledger write stays open. A process of the same user that swaps the leaf in it is recorded as the owner of a link it did not write. The ledger fingerprint then no longer matches the live link, so a later removal is refused. This is the same check-then-act limit as for `replace_link`.

## Merge

A sensitive co-owned file (`claude-settings/v1`) runs through `try_merge` (`merge_read`, `merge_decide`, `merge_settle`), not the normal path. There is no output hash, no `path:` content hash and no ledger fingerprint. Adoption and merge never apply to one path: adoption is for a normal-class link, and a merge output is sensitive. For one operation, in this order:

1. **Approval.** An operation with `takeover = true` (the first write into an existing co-owned file with no ledger entry) needs `--allow-adopt '<id>'` like an adoption. It is part of the gates: without it apply exits 3 with `E_ADOPT_UNAPPROVED` before any write. The approval also covers the handler entries already in the file that are identical to a declared handler: the merge records them as owned ([docs/adapters.md](adapters.md), "Merge rules", rule 4), so a later `retire` removes them. The receipt records `how = allow_adopt`. Any other merge operation, `retire` included, needs only `--yes-plan`: it is neither a removal nor an adoption, and `retire` deletes no file.
2. **Prerequisites** as for any operation. The `path:` precondition of a sensitive file holds its mode and no hash or size.
3. **Topology**: the parent chain is opened without following a link.
4. **Re-read under the lock** (`Dir::read_live`): one descriptor opened without following a link. The file must be regular, owned by the user, with one hard link, at most 16 MiB, and the stamp before and after the read must agree (device, inode, size, modification time, change time). A mode open to group or others is `E_CONFLICT` (blocked, exit 3). The failpoint `merge-before-read` sits before this read.
5. **Merge** (`Generator::merge`) with `GeneratorInputs.live` (the bytes just read, or `None`), `owned_entries` and `owned_appended` (from the ledger) and, when the resource declares a fragment directory, the fragments. Apply reads them like those of an assembly (`gather`): the planned list, each file below the opened directory with `Dir::read_input`, a foreign fragment compared with the token the plan recorded and a link producer's source compared with the plan. A change is `E_PLAN_STALE`. A fragment that is not a JSON object is `E_CONFLICT`. Both block and write nothing. A `retire` reads no fragment. A malformed or foreign-shaped file is `E_CONFLICT` and blocks. The result is made from the bytes just read, not from the plan-time file.
6. **Conflict check**: `dfm_model::merge::check_foreign`, the same function plan time uses ([docs/adapters.md](adapters.md), "Conflict detection"). A foreign edit is `E_CONFLICT`, blocked, nothing written. A lost salt with a recorded hash is `E_STATE_INVALID`, blocked. An absent live file skips the comparison. `retire` skips the check and does not read `state/salt`: it removes owned entries by the hash of the declared entry and touches no managed field, so a lost or unusable salt does not stop it.
7. **Salt**: `state/salt` is created if it does not exist, after the last look at the signal record and just before the intent entry, so an operation that ends as `not_started` has not created it.
8. **Intent**: the journal gets an `intent` entry with `existed_before` and no expected hash.
9. **Effect**: if the bytes differ from the live file, publish them. A new file uses an exclusive rename (`RENAME_EXCL`), which never replaces. An existing file uses `renameat` after a last identity check (`Publish::ReplaceStamp`). If the bytes are equal nothing is written, and the ledger is still updated. The failpoints are `apply-before-effect`, `file-after-tmp`, `file-after-hash` (a replace, between the content check and the owner check) and `apply-after-effect`.
10. **Read-back**: the file is always read and compared with the bytes in memory, also when nothing was written (an unchanged merge or a takeover). The mode must be 0600 after a write, and the mode read at step 4 when nothing was written. A file rewritten or changed with `chmod` between the read and this step fails the postcondition (`E_IO_FAILED`, journal `failed`), also when the merge changed nothing, and the ledger is not written.
11. **Outcome and ledger**: the journal gets `done`. The ledger records the output with no fingerprint, the salted hash of the managed fields as written (fragments included), the owned handler entries and the appended array items. A `retire` drops the output from the ledger.

The receipt operation has class `sensitive`, no `before` and no `after`, the output path as owned (not for `retire`), and `reload = not_attempted` when a file was published. The receipt validator accepts that shape for a sensitive operation. The report prints the same line as for any operation.

## Journal ids

The planner's `journal_id` ignores observations. A rerun after a partial apply plans the same id. Apply forms the id it writes as `<journal_id>-<n>`, where `n` is one more than the number of `intent` entries already in the journal whose id starts with `<journal_id>-`. The lock is held, so the count cannot race. Every attempt has a distinct id and an interrupted attempt keeps its record. The receipt uses the attempt id for an operation that reached its intent, and the planner id for one that did not.

## Interrupted runs

Settling runs after the gates and before the first operation. It first repairs a torn final journal line (no terminating newline, from a crash or a failed append): it cuts the line off and journals a `failed` entry with reason "torn line repaired: removed N bytes" (`Store::repair_torn_journal`). That entry is the first write of the run. A torn line that is not the last one is not repaired and the run refuses with `E_STATE_INVALID`. Then settling has two steps:

1. An `intent` entry without a terminal entry is an interruption record. Apply asks the operation's `already_done` check (`Store::reconcile`): the live entry is read through the opened HOME and compared with the recorded expectation. The result is journaled `done`, `failed` or `unknown`. For `update_file` and `replace_link`, a live entry that still equals the ledger fingerprint means the effect did not happen (`failed`). A live entry that equals the expected new value is `done`, and a ledger entry that still holds the old fingerprint for that path is moved to the new one first. Apply runs nothing again and creates no new ownership in this step.
2. Every `unknown` entry that no later entry settled is probed again, also one from an earlier run, except those step 1 just wrote. The outcomes, and when a `done` records ownership, are in [docs/state.md](state.md), rule 5. An entry that stays open blocks its path, its ancestors and its descendants (compared case-folded) for this run: each operation on such a path is `blocked` with `E_OP_INTERRUPTED`, and the text names the journal id and the fix. An entry with no intent record, or an intent that lists no path, blocks every operation of its resource instead, and only `state resolve <journal-id> --as failed` clears it.

`state recover` runs this settling alone (no plan, no allowance flag, no receipt). `state resolve` settles one entry by hand ([docs/state.md](state.md)). Both install no signal handler, so a signal ends them at once. Each journal entry is one durable append, so a stop between two entries leaves a state that the next run settles again. A signal that `apply` records before settling skips the settling: no entry is touched and the run reports `not_started`.

Settling writes the journal, so it counts as a mutation. A blocked operation stops the run like any other block. The planner does not read the journal, so plan and apply agree on the operation list and apply adds the block.

## Unconfirmed durability

A primitive that committed its effect but could not confirm durability returns `E_IO_UNCONFIRMED` (`make_dir`, `make_link`, `replace_link`, `publish_file` and `remove_entry`). The executor does not call that a failure. The operation is `unknown`: the journal gets an `unknown` entry, the path is blocked for the rest of the run, dependents are `blocked`, no ownership is recorded, and the run stops. The effect is in place. The entry stays open in later runs: each apply probes it again (Interrupted runs), and `doctor` lists it until it is settled.

## Failure and accounting

- The first operation that fails or blocks stops all further mutation. There is no `--continue`.
- An operation that depends on it (through `after`, transitively through results) is `blocked`. Every other operation that has not started is `not_started`. `current`, `held` and `not_in_scope` operations are reported as they are.
- Completed operations stay `updated`. There is no rollback.
- The report lists one line per operation and the counts `updated`, `current`, `failed`, `blocked`, `unknown`, `not_started`, `not_in_scope`, `held`, `pending`, `blocked_pending`, so the counts add up to the number of operations. These are the contract status names, the same as in the receipt.
- The first problem keeps its own code (`E_IO_FAILED`, `E_PLAN_STALE`, `E_CONFLICT`, `E_IO_UNCONFIRMED`, and so on). There is no umbrella code. When the store wrote anything, the refusal is marked `after_mutation`, which turns a block code into exit 1.

| Result | Exit |
| --- | --- |
| Everything updated, current or not in scope | 0 |
| A block (`E_PLAN_STALE`, `E_CONFLICT`, `E_PARENT_UNSAFE`, any gate) before anything was written | 3 |
| A failure or block after anything was written (any journal entry or effect) | 1, with the original code |
| An I/O failure (`E_IO_FAILED`, `E_IO_UNCONFIRMED`) even before anything was written | 1, because the code is a failure code in the registry |
| Usage | 2 |

An I/O failure before the first write therefore prints `result: failed`, not `blocked`. The printed result and the exit come from one value.

A receipt is written whenever the run wrote anything, also after a failure. If settling earlier work fails, or a clock or receipt step fails after a write, the run still prints its report and writes the receipt as far as it can. The exit stays 1. Its `result.outcome` and `exit_code` follow architecture section 5. A run that wrote nothing writes no receipt. A rerun of an applied plan without `--yes-plan` ends at step 4 of "The command". With `--yes-plan` it writes no journal entry, ledger or receipt and exits 0, but it takes and releases the lock, which rewrites the lock holder note.

## Receipt fields apply adds

- `operations[].reload`: see Publication and reload. The contract's receipt shape does not have it. No schema bump was made for it.
- `approvals[{operation_id, how, at}]`: one entry per `--allow-disruptive` operation that ran (`how = allow_disruptive`) and per `--allow-adopt` operation that reached its intent (`how = allow_adopt`). `operation_id` is the id of the receipt operation (the journal attempt id). The validator checks that `how` fits the operation ([docs/state.md](state.md), "Documents"). Absent when there is none.
- `sources[{id, tree}]`: a path source has no `commit`, and a source with no commit is a path source. `id` is `path-` and the first 12 hex characters of the tree digest. The tree digest is the one the planner computes, so the plan digest covers it.
- Every operation that ran has the journal attempt id as its receipt `id`, also when it failed, was unknown or was blocked after its intent. An operation that did not reach its intent has the planner's journal id.

## Publication and reload

Publishing a file does not reload the program that reads it (architecture section 8, "A correct file, an installed executable and a reloaded consumer are three separate postconditions"). Each link or file `apply` publishes has `reload = not_attempted` in its receipt operation, and the report prints a line that says no reload happened. A test asserts that no published file claims a reload. The receipt validator also accepts `requested` and `reloaded`, which only a service operation writes ([docs/lifecycle.md](lifecycle.md)). The contract names `unsupported` and `failed` are refused, because nothing can write them.

## Decisions where the documents were silent

| Question | Choice |
| --- | --- |
| An output that already is correct but unrecorded (`unrecorded_current`) | A link to a file that no ledger entry mentions is planned as `adopt`. Anything else stays as it is and is not claimed. A regular file is never adopted. |
| New codes | `E_APPROVAL_REQUIRED`, `E_GENERATOR_UNAVAILABLE`, `E_SENSITIVE_UNSUPPORTED`, `E_DISRUPTIVE_UNAPPROVED`, `E_ADOPT_UNAPPROVED`. All exit 3. |
| One ledger entry behind a link that leaves HOME (stow style) | Blocks only the operations on that entry. A removal candidate gets `E_PARENT_UNSAFE`. A claim gets `E_TARGET_RESERVED` (the store's check). The plan has a blocker, so apply refuses it (exit 3), but other operations stay plannable and the machine as a whole is not `E_STATE_INVALID`. |

## Not built

- A stored plan and a `--plan <file>` flag. The digest is the contract.
- `--json`, `--no-input`, `--timeout`, an interactive approval and a run limit. Every run is noninteractive, so exit 4 does not occur.
- A configurable directory mode policy. Directories apply creates are 0700.
- `Fs` and `Clock` traits, adopting a regular file or a directory link, and rename-aside hardening for the check-then-act windows below.
- A command that removes one ledger entry by hand.

## Signals

`apply` (not `--preview`) handles SIGINT and SIGTERM. It installs the handlers after it takes the writer lock and plans again under the lock, and before it settles earlier work. The lock file can exist before the handlers do. No journal, ledger or HOME write happens in that window, and a signal in it ends the process with the default action. The handler stores the number of the first signal in an atomic and does nothing else. The executor looks at the record before it settles, before each operation, and again immediately before it writes the `intent` entry (after the checks, the link source walk and the generation). A signal seen at that last look ends the operation as `not_started` with no `intent` entry:

- The operation in flight finishes: its effect, read-back, outcome entry and ledger write. A staged file is never left half written.
- No new `intent` entry is written. Every operation that has not started is `not_started`. An operation that needs nothing (`current`, `held`, `not_in_scope`) is reported as it is.
- The receipt has outcome `interrupted` (SIGINT, exit 130) or `terminated` (SIGTERM, exit 143), as for any other run that changed something. A run that changed nothing writes no receipt, and a signal before the first mutation exits 130 or 143 without one.
- The report prints, then the last line is `result: interrupted` or `result: terminated`. The process returns the code. The handler never exits the process.
- A signal has precedence over a failure in the exit code. The operations still show what failed.
- A signal that arrived before a pre-run check refused the plan still gives 130 or 143.
- If the receipt cannot be written, the report says `receipt: not written (...)` and the exit is still 130 or 143.
- A second signal, of either kind, does nothing. The first signal keeps its code and the receipt is still written.
- A signal that arrives while the receipt is written cannot be in that receipt. The exit is still 130 or 143, the report adds a `signal:` line, and the receipt keeps the outcome it had.
- If the parent set a signal to ignore (`nohup`, a background job), the handler for that signal is not installed and it stays ignored.
- `plan`, `doctor`, `explain`, `state` and `apply --preview` install no handler, so a signal ends them as before: the kernel kills the process and no receipt exists.

The handlers are installed with `SA_RESTART`. A blocking call restarts, and the executor sees the record at the next check, so a file system call never reports a spurious `EINTR` as a failed operation. `apply` starts no child process after the handlers are installed. `secrets`, `packages`, `services` and `trial` install the handlers before their first child (also with `--preview`), so the runner can poll the stop record while a child runs and end the child's process group with SIGTERM, a 10 s grace period and SIGKILL ([docs/secrets.md](secrets.md), [docs/packages.md](packages.md)). A signal recorded before the lock ends those runs with exit 130 or 143 and no write. The 30 s wait of the target contract (architecture section 10) is not built.

SIGKILL, any signal with no handler (SIGHUP, SIGQUIT, ...), a crash or a power loss ends the process with no cleanup. It can leave an `intent` entry, and the next run settles it (Interrupted runs), except an `intent` for a sensitive merge (see Known limits, "Merge crash recovery").

## Known limits

- **Clock.** The run reads the clock at its start. If the clock fails then, the run does not start. If it fails later, the last good reading is reused. No 1970 stamp is written.
- **Merge check-then-act.** Between the identity check and `renameat` a write by another process is not detected and is replaced by a merge of the older content. A manager lock does not lock the application ([docs/adapters.md](adapters.md), "Check-then-act limit"). The identity check does catch a file replaced or rewritten between the re-read and the check, with a test for each.
- **Merge crash recovery.** A crash after the rename and before the outcome entry leaves an `intent` for a sensitive file. `reconcile` has no expected hash for it, so the answer is `unknown`. The entry stays open and blocks that path with `E_OP_INTERRUPTED` in every later run until `dfm state resolve <journal-id> --as failed` settles it. After that, the next run plans from the live file and merges again. The ledger may then lack the salted hash for a file the manager did write: the next merge treats the file as one with no ledger entry and takes over the managed fields.
- **Lost salt.** If `state/salt` is lost while the ledger holds a salted hash, the foreign-edit check cannot run and the plan is blocked (`E_STATE_INVALID`) until the file is restored ([docs/state.md](state.md), "Documents").
- **Replace and remove check, then act.** `replace_link`, `update_file` and `remove` verify the leaf and then call `renameat` or `unlinkat`. These are two system calls. A process of the same user that swaps the leaf between them wins. In a parent that is group or world writable, another user can also win in that window: the owner and inode checks narrow it and do not close it. Create operations have no such window (`EEXIST`). A swap of a parent directory cannot redirect any step, because every step uses the opened descriptor. A parent swapped after it was opened is not detected for the operations that already hold it. The effect lands in the directory that was verified, and the next operation sees the changed topology and is blocked.
- **Source content during the run.** The `source:` preconditions are checked when the plan is made and again under the lock. A link reads its source again immediately before its intent entry, and a generated file reads its sources again immediately before it is generated. A source changed after that point is not compared again: a link publishes a path, so a later change shows only in the next plan.
- **Crash between the effect and the ledger (create).** For a new output, the output exists, the journal says `done` (or `intent` only), and the ledger has no entry. `update_file` and `replace_link` do not have this gap: the ledger moves before `done`, and recovery repairs the fingerprint after a crash. The next plan offers an exact link as an `adopt` operation, and `--allow-adopt` records it. Until then the output stays and is not owned, so it is never removed. This is the safe direction. A regular file with the right content is not recovered.
- **Forged ledger.** The ledger is not authenticated. A ledger edited by hand, with correct hashes for a user's own regular files, can make apply delete those files when the matching `--allow-disruptive` ids are given. The per-id flag and a review of the plan (which lists every removal) limit this. A stronger fix is a per-machine MAC over the ledger, or a device and inode record per output. Neither is built.
- **Receipt write after the work.** If the receipt cannot be written, the run exits 1 and the journal and ledger are the record.
- **Case and Unicode aliases** are handled by the planner ([docs/planner.md](planner.md), rules 1 and 3). Apply trusts the plan's alias check and the byte-exact precondition strings.
- **Privilege.** Apply never escalates. A path that needs another owner is a planner blocker, and apply checks the leaf owner again (Order, step 3). A parent directory that is group or world writable is still accepted.
- **Evidence scope.** The tests use the `test-isolation` build and temporary directories.
