# Durable state

Task: DFM-17. This document records what the first state implementation does and where it resolves or differs from [docs/architecture.md](architecture.md) (the contract). The contract wins where this document is silent.

## What exists

| Part | Location |
| --- | --- |
| Pure document types, strict parsing, redaction, time formatting | `crates/model/src/state.rs` |
| Storage validation, writer lock, atomic writes, journal, ledger, receipts, trials, snapshots, reconciliation | `crates/engine/` (`dfm-engine`, depends on `dfm-model` only) |
| `bind` initializes state. `state check` verifies identity. | `crates/cli/` |
| Recording commands for tests (`__record-op`, `__record-trial`, `__reconcile`) | `crates/cli/src/state_cmd/harness.rs`, compiled only with `test-isolation` |

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
- Ledger: one entry per resource with its exact `owned_outputs`. Two resources cannot own one path. Paths are normalized absolute paths inside HOME and outside the manager's own storage and configuration. A normal resource has a SHA-256 fingerprint. A sensitive resource has none (see limits). `authorizes_delete` needs a normal entry for that resource and path whose fingerprint equals the live one. A mismatch, a missing entry or a sensitive resource authorizes nothing.
- Receipt: the contract fields. `counts` has one key per status and must equal the tally of `operations`. A sensitive operation carries no `before` or `after`. `error` is bounded to 4 KiB.
- Journal: `intent` (with `recovery{outputs[{path, existed_before, expected_sha256?}], snapshot?}`), then one of `done`, `failed`, `blocked`, `pending`, `unknown`. An `intent` without a later outcome for the same `(op_id, op)` is an interruption record.
- Trial: `trial_id`, `tool`, `baseline{tool_preexisting, paths[{path, existed, sha256?}]}`, `owned_outputs`.

## Rules that the code enforces

1. **Topology (C08).** HOME is resolved through every alias (`/var` to `/private/var`, a link above HOME). Below HOME, every component of the config directory, the lock directory and the state root must be a real directory (never a symlink, never another file type) owned by the current user. State root and below must be exactly 0700. Existing shared parents and the config directory must not be group or world writable. Files are opened with `O_NOFOLLOW`, must be regular, owned by the current user and 0600. A violation is `E_STORAGE_INVALID` (exit 3). Nothing is repaired or replaced.
2. **Writer lock (C01, C05).** Mutating paths take an exclusive `flock` on the lock file without waiting. A second holder gets `E_LOCKED` (exit 3). The kernel releases the lock when the process ends, so an abrupt exit cannot leave it held. Read-only commands take no lock.
3. **Atomic persistence (C01, C05).** Replacement is a same-directory 0600 temporary file, `fsync`, `rename`, directory `fsync`. A failure before the rename leaves the previous file. Receipts and snapshots use create-only publication (`hard_link`) and are never overwritten. The journal is append-only with `fsync` and refuses to extend a file whose last line is torn.
4. **Intent before effect (C02).** The harness order is: intent with recovery metadata (`fsync`), effect, independent read-back, outcome entry, ledger update, receipt. Ownership is recorded only after the outcome entry.
5. **Reconciliation (C02, C03, C05).** `reconcile` asks a probe (the operation's `already_done` check) and journals `done`, `failed` or `unknown`. It never runs the operation again, never edits the ledger and never creates ownership. An interrupted operation becomes owned only when a later normal run records it.
6. **Init (C01).** `state.next/` is built and renamed to `state/`. A crash leaves no `state/` or a complete one. An existing `state/` that is not an empty directory is never recreated: lost or damaged state needs inspect-and-adopt.
7. **Bind order (C07, C11).** Phase 1 is read-only and refuses a different identity (in the binding or in `state.json`) with `E_REBIND_UNSUPPORTED` before any write, including the lock file. Phase 2 takes the lock, decides again, writes `state/` first and the binding second. A crash between them can only be completed by the same identity. There is no force flag and no command infers a profile.
8. **State check (C06).** `dotfiles-manager state check` is read-only. It loads every document and reports each problem by file: `E_IDENTITY_MISMATCH` for a readable document written for another identity, the document's own code for an unreadable one (it hides nothing else), `E_OP_INTERRUPTED` for an intent without an outcome. Exit 1 with `E_DIAGNOSED` when there are findings, 0 otherwise.

## Decisions and ambiguities resolved

| Contract text | Reading used |
| --- | --- |
| Section 4: `state/ledger.json` under "State root: `~/.local/state/dotfiles-manager/`" | Files live in `<root>/state/`. See Layout. |
| Section 5 receipt: `profile{id, ...}` and `class` per operation | `profile.id` is the canonical profile path, the binding's identity. `class` is the sensitivity class (`normal` or `sensitive`), because the receipt rule for sensitive operations keys on it. |
| Section 10: a second mutating command gets exit 3 `E_LOCKED` | `bind` follows it. The DFM-14 concurrent-bind test now accepts `E_LOCKED` as a loser's outcome. At least one bind still wins. |
| Section 8, "Filesystem writes": descriptor-relative `openat`/`renameat` | Not met. `unsafe_code` is forbidden and `std` has no `openat`. The engine walks components with `lstat` and uses `O_NOFOLLOW`. A component swapped between check and use is a residual window. Closing it needs `libc` or `rustix`, a new dependency this task does not add. |
| Section 14 decision 3 accepts `serde_json` | Added, pinned `=1.0.149`, used by `dfm-model` only. |

## Limits (not implemented, by design of this task)

- **State upgrade (C03).** Every kind has "Upgrades from: none yet", so there is nothing to convert and the `state.next`/`state.prev-<n>` upgrade procedure is not built. A newer or unsupported state is refused with its code, and nothing is rewritten. `verify --mark`, `retire-previous` and `state/UPGRADE` are not built.
- **Sensitive paths (C04).** The per-machine salted hash of section 8 item 7 needs a salt and a producer adapter. Neither exists. The ledger therefore stores no hash for a sensitive resource, and `authorizes_delete` refuses such a resource. A sensitive resource has no snapshot content (`write_snapshot` refuses it). Redaction removes known secret values (longest first), escapes control characters and bounds length. It does not detect unknown credential patterns.
- **Application-owned data (C04).** The engine accepts only exact paths inside HOME and outside its own storage. Deciding that a path inside HOME belongs to an application is the adapter's evidence, not the engine's.
- Snapshot retention and pruning are not built. Snapshots can be written and are private.
- `grants.json`, the journal rotation policy and the `Fs`/`Clock` ports are not built.
- No device or native behavior is verified. Every test runs in an isolated HOME.

## Recovery by hand

- `E_STATE_INVALID`, `E_STATE_NEWER`, `E_SCHEMA_UNSUPPORTED` on a document: keep the file. Use the binary version that wrote it, or restore from a copy. No command rewrites it.
- `E_REBIND_UNSUPPORTED` against existing state: the state directory belongs to another identity. Move `~/.local/state/dotfiles-manager/` aside yourself if that machine identity really is retired.
- `E_OP_INTERRUPTED`: a later run decides with the operation's `already_done` check. The isolated test binary has `__reconcile` for this.
