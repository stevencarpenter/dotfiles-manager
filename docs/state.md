# Durable state

Task: DFM-17. This document records what the first state implementation does and where it resolves or differs from [docs/architecture.md](architecture.md) (the contract). The contract wins where this document is silent.

## What exists

| Part | Location |
| --- | --- |
| Pure document types, strict parsing, redaction, time formatting | `crates/model/src/state.rs` |
| Storage validation, writer lock, atomic writes, journal, ledger, receipts, trials, snapshots, reconciliation | `crates/engine/` (`dfm-engine`, depends on `dfm-model` only) |
| `bind` initializes state. `state check` verifies identity. | `crates/cli/` |
| Recording commands for tests (`__record-op`, `__record-trial`, `__reconcile`) | `crates/cli/src/state_cmd/harness.rs`, compiled only with `test-isolation` |
| Failure injection (error, abort, wait for a file) | `crates/engine/src/fail.rs`, behind the engine feature `failpoints`, which only `dfm-cli/test-isolation` enables. A gate test rejects it in a release dependency graph. |

`ports`, `adapters` and `platform_macos` trait layers are not created. The engine takes the OS facts it needs (user id, open flags) from the caller in `Sys`, so it names no OS constant.

No workflow (`sync`, `update`, `try`, `adopt`, `purge`) exists. Nothing in the release binary writes a ledger entry, receipt or trial record yet. The engine API does, and the isolated test binary drives it.

## Layout

```
~/.config/dotfiles-manager/machine.toml            binding (DFM-14)
~/.local/state/dotfiles-manager.lock               writer lock (flock), outside the state root
~/.local/state/dotfiles-manager/state/             0700
    state.json   ledger.json   journal.jsonl       0600
    receipts/ snapshots/ trials/                   0700
~/.local/state/dotfiles-manager/state.next/        only during init
```

The architecture table lists `state/ledger.json` relative to the state root `~/.local/state/dotfiles-manager/`. This implementation reads that literally: the files sit in a `state/` directory inside the root. That is also the only reading under which `state.prev-<n>` and `state.next` are siblings of `state` and the lock "in the parent directory" is outside it.

`grants.json` and the `state upgrade` machinery are not created. Trust grants belong to a later task.

## Documents

All are JSON in canonical form (UTF-8, sorted keys, no extra whitespace), at most 16 MiB (journal lines 64 KiB), nesting depth at most 32, unknown keys refused. The schema is read first: `1` is accepted, a higher integer is `E_STATE_NEWER`, any other integer is `E_SCHEMA_UNSUPPORTED`, a missing one is `E_STATE_INVALID`.

- `state.json`, `ledger.json` and each trial carry `binding{profile, machine_id, binding_digest}`. The receipt carries `profile{id, binding_digest}` and `machine_id` as the contract lists them.
- **Identity is profile plus machine id.** `binding_digest` is audit evidence and is never compared, because an authored binding edit changes it without changing the owner.
- Every document written for another identity than `state.json` is refused on write (ledger, receipt, trial) and reported on read.
- Ledger: one entry per resource with its exact `owned_outputs`. Two resources cannot own one path. Paths are normalized absolute paths inside HOME, checked on read as well as on write. They may not be inside, equal to, or an ancestor of the state root, the config directory, the lock file, the data directory (toolchains) or the cache directory (build). Comparison is case-folded (APFS). On write, symlinks in the parent directories are resolved first. A hand-written ledger that violates this is a finding and grants nothing. A normal resource has a SHA-256 fingerprint. A sensitive resource has none (see limits). `authorizes_delete` needs a normal entry for that resource and path whose fingerprint equals the live one. A mismatch, a missing entry or a sensitive resource authorizes nothing.
- Receipt: the contract fields. `counts` has one key per status and must equal the tally of `operations`. A sensitive operation carries no `before` or `after`. `error` is bounded to 4 KiB.
- Journal: `intent` (with `recovery{outputs[{path, existed_before, expected_sha256?}], snapshot?}`), then one of `done`, `failed`, `blocked`, `pending`, `unknown`. An `intent` without a later outcome for the same `(op_id, op)` is an interruption record.
- Trial: `trial_id`, `tool`, `baseline{tool_preexisting, paths[{path, existed, sha256?}]}`, `owned_outputs`.

## Rules that the code enforces

1. **Topology (C08).** HOME is resolved through every alias (`/var` to `/private/var`, a link above HOME). Below HOME, every component of the config directory, the lock directory and the state root must be a real directory (never a symlink, never another file type) owned by the current user. State root and below must be exactly 0700. Existing shared parents and the config directory must not be group or world writable. Files are opened with `O_NOFOLLOW`, must be regular, owned by the current user and 0600. A violation is `E_STORAGE_INVALID` (exit 3). Nothing is repaired or replaced.
2. **Writer lock (C01, C05).** Mutating paths take an exclusive `flock` on the lock file without waiting. A second holder gets `E_LOCKED` (exit 3). The kernel releases the lock when the process ends, so an abrupt exit cannot leave it held. Read-only commands take no lock. The lock file opens non-blocking and must be a regular file owned by the user with mode 0600 (a mode the umask tightened is set to 0600). The holder compares the locked file with the path before every write. If the file was removed or replaced, the holder refuses to write (`E_LOCKED`). A second command can still lock the new file, so removing the lock file lets a second writer start, but the first writer is fenced and writes nothing more.
3. **Atomic persistence (C01, C05).** Replacement is a same-directory 0600 temporary file, `fsync`, `rename`, directory `fsync`. A failure before the rename leaves the previous file. A failure after it is `E_IO_UNCONFIRMED` (exit 1): the new file is in place and only its durability is unknown. New directories and the journal are `fsync`ed, with their parents. Receipts and snapshots use create-only publication (`hard_link`) and are never overwritten. The journal is append-only with `fsync` and refuses to extend a file whose last line is torn.
4. **Intent before effect (C02).** The harness order is: intent with recovery metadata (`fsync`), effect, independent read-back, outcome entry, ledger update, receipt. Ownership is recorded only after the outcome entry.
5. **Reconciliation (C02, C03, C05).** `reconcile` asks a probe (the operation's `already_done` check) and journals `done`, `failed` or `unknown`. It never runs the operation again, never edits the ledger and never creates ownership. An interrupted operation becomes owned only when a later normal run records it.
6. **Init (C01).** `state.next/` is built and renamed to `state/`. A crash leaves no `state/` or a complete one. An existing `state/` that is not an empty directory is never recreated: lost or damaged state needs inspect-and-adopt.
7. **Bind order (C07, C11).** Phase 1 is read-only and refuses a different identity (in the binding or in `state.json`) with `E_REBIND_UNSUPPORTED` before any write, including the lock file. Phase 2 takes the lock, decides again, writes `state/` first and the binding second. A crash between them can only be completed by the same identity. There is no force flag and no command infers a profile.
8. **State check (C06).** `dotfiles-manager state check` is read-only. Topology and directory-level refusals are findings with exit 1, like a diagnostic command, not an abort. It loads every document and reports each problem by file: `E_IDENTITY_MISMATCH` for a readable document written for another identity, the document's own code for an unreadable one (it hides nothing else), `E_OP_INTERRUPTED` for an intent without an outcome. Listing errors, unreadable directories and stray temporary files (including in `snapshots/`) are reported too. Exit 1 with `E_DIAGNOSED` when there are findings, 0 otherwise.
9. **Exit after a mutation.** A refusal that happens after a mutation began exits 1, not 3 (section 10). `Refusal::after_mutation` marks it. `bind` marks refusals after it created the state directory, and the harness marks everything after the intent entry.
10. **Re-check before write.** Before each ledger, journal, receipt, trial or snapshot write the engine checks the lock file and walks the directory chain again. This narrows the window between validation and use. It does not close it.
11. **Redaction.** One pass over every persisted descriptive field (receipt command, version, commit, approval kind, operation error; journal reason and token). Overlapping matches merge. Control, bidirectional and zero-width characters become visible escapes. Length is bounded to 4 KiB. A resource, adapter, id or path that contains a known secret is refused, not redacted, because redaction would break its use. The refusal text does not repeat the value.

## Decisions and ambiguities resolved

| Contract text | Reading used |
| --- | --- |
| Section 4: `state/ledger.json` under "State root: `~/.local/state/dotfiles-manager/`" | Files live in `<root>/state/`. See Layout. |
| Section 5 receipt: `profile{id, ...}` and `class` per operation | `profile.id` is the canonical profile path, the binding's identity. `class` is the sensitivity class (`normal` or `sensitive`), because the receipt rule for sensitive operations keys on it. |
| Section 10: a second mutating command gets exit 3 `E_LOCKED` | `bind` follows it. The DFM-14 concurrent-bind test now accepts `E_LOCKED` as a loser's outcome. At least one bind still wins. |
| Section 8, "Filesystem writes": descriptor-relative `openat`/`renameat` | Not met. `unsafe_code` is forbidden and `std` has no `openat`. The engine walks components with `lstat` and uses `O_NOFOLLOW`. A component swapped between check and use is a residual window. Closing it needs `libc` or `rustix`, a new dependency this task does not add. |
| Section 14 decision 3 accepts `serde_json` | Added, pinned `=1.0.149`, used by `dfm-model` only. |

## Known limits

Owner decisions are pending on the first four items. They are not worked around here.

- **Descriptor-relative writes (C08).** Not built. `unsafe_code` is forbidden and `std` has no `openat`. The engine walks components with `lstat`, opens with `O_NOFOLLOW`, and re-checks the chain before each write. A component swapped between the last check and the use is a residual race. Closing it needs `libc` or `rustix`. **C08 is not claimed as fully met.**
- **State upgrade (C03).** Not built. Every kind has "Upgrades from: none yet", so there is nothing to convert, and the `state.next` / `state.prev-<n>` upgrade procedure, `verify --mark`, `retire-previous` and `state/UPGRADE` do not exist. A newer or unsupported state is refused with its code and nothing is rewritten.
- **Conflict detection for sensitive paths (C04).** Absent. The per-machine salted hash of section 8 item 7 needs a salt and a producer adapter. Neither exists, so the ledger stores no hash for a sensitive resource, `authorizes_delete` refuses such a resource, and an edit to a sensitive output is not detected.
- **Trial baseline class (S6).** A trial baseline has no sensitivity class. A baseline path hash is stored for any path.
- **Bind orchestration (R8).** `bind` sequencing lives in the CLI crate, not the engine.
- **Strict-parser gaps (R12).** Known gaps remain in how strictly the parsers reject unusual but valid JSON (for example duplicate keys, which `serde_json` accepts, last one wins).
- **Application-owned data.** The engine accepts only exact paths inside HOME and outside its own paths. Deciding that a path inside HOME belongs to an application is the adapter's evidence.
- **Redaction** removes known secret values only. It does not detect unknown credential patterns. Unicode normalization differences are not folded in the path scope.
- Snapshot retention and pruning, `grants.json`, journal rotation and the `Fs`/`Clock` ports are not built.
- A non-UTF-8 entry name is reported as a finding by code, but it has no test, because APFS rejects such names.
- **Evidence scope.** The "installed CLI" evidence is the `test-isolation` build driven through `--home`, the same binary used by the other CLI tests. It is not the installed release binary, and no device was used. The recording commands exist only in that build.

## Recovery by hand

- `E_STATE_INVALID`, `E_STATE_NEWER`, `E_SCHEMA_UNSUPPORTED` on a document: keep the file. Use the binary version that wrote it, or restore from a copy. No command rewrites it.
- `E_REBIND_UNSUPPORTED` against existing state: the state directory belongs to another identity. Move `~/.local/state/dotfiles-manager/` aside yourself if that machine identity really is retired.
- `E_OP_INTERRUPTED`: a later run decides with the operation's `already_done` check. The isolated test binary has `__reconcile` for this.
