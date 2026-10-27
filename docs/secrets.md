# Secrets

The `op-template/v1` adapter and the `secrets` command. This document describes the behavior at HEAD.

| Part | Location |
| --- | --- |
| `Scope` (`config`, `secrets`, `packages`, `services`) and the secret decision (`decide_secret`) | `crates/model/src/plan/build.rs` |
| Plan-time verdict on the destination (`secret_facts`), the session check (`credentials`) and the render (`render`) | `crates/cli/src/adapters/secret.rs` |
| The bounded child-process runner | `crates/engine/src/proc.rs`, `crates/engine/src/signal.rs` (`kill_child`) |
| `Generator::renders` and `Generator::render` | `crates/engine/src/generate.rs` |
| Execution (`try_render`) | `crates/engine/src/apply.rs` |
| `secrets` command (`apply` over the secrets scope) | `crates/cli/src/apply_cmd.rs`, `crates/cli/src/lib.rs` |
| Tests with an `op` look-alike, and the runner's own tests | `crates/cli/tests/secrets.rs`, `crates/cli/tests/runner.rs` |

## The command

```
dfm secrets [--root <dir>] [--preview] [-y|--yes-plan <digest>] [--allow-adopt <operation-id>]...
```

`secrets` is `apply` over the secrets scope (docs/apply.md). Every `secret` resource is planned. Every other resource is `not_in_scope` ("link is configuration, outside a secrets plan"). `--root` defaults to the root the binding records. `plan` and `apply` list a secret as `not_in_scope` with destination and mode only. They never read the destination or start `op`.

Order of one run:

1. Resolve, observe, plan. The plan prints with its digest. A blocked plan exits 3.
2. The gates that need no write (docs/apply.md). Then the credential gate: for each account a planned secret names, `op whoami --account <account>` must exit 0. Otherwise the run exits 3 with `E_CREDENTIAL_UNAVAILABLE`, before the lock and before any write. An `op` that is missing from the search path or fails its executable checks is `E_PROBE_FAILED`, also exit 3. `--preview` runs this gate too, so it exits 0 only when `secrets` would render.
3. Without `--yes-plan`: exit 3, `E_APPROVAL_REQUIRED`. With a digest that differs: `E_PLAN_STALE`. A plan with no planned secret needs no digest and exits 0 (docs/apply.md, step 4).
4. The writer lock, the second plan, settling, then one operation per secret (below). Receipt and report as for `apply`, with `command = secrets`.

`--allow-disruptive` is a usage error (exit 2): a secrets plan removes nothing. A deselected secret keeps its file and its ledger entry. The configuration plan lists it as `current` ("kept with its ledger entry") and plans no removal. Delete the file by hand if it must go.

## The adapter

The `op-template/v1` value has three fields:

- `template`: a source file of the module, relative to the manifest.
- `account`: the 1Password account. Letters, digits, `.`, `_`, `-`, `@`, not starting with `-`, at most 128 characters.
- `mode`: an integer file mode with group and other bits clear and owner read kept, because the manager reads the file back after a write and at every plan. `0o600` and `0o400` pass. `0o200` and `0` are `E_MANIFEST_INVALID`.

The resolver records no content identity for the template, and `explain` prints `[redacted]` for the values (docs/resolver.md item 12).

The render is `op inject --account <account>`, with the template bytes on standard input and the result on standard output. No `-i` or `-o` flag is used, so `op` never opens a path the manager did not read. The result exists only in memory until it is published. The template is read from the plan's source root as any source is: below the root without following a link, a regular file of the user with one hard link, at most 1 MiB. It must be byte for byte the template the locked plan was resolved from. The `secrets` command keeps the bytes it loaded, and the render refuses any other bytes (`E_PLAN_STALE`, blocked, nothing rendered).

## How `op` is run

`secrets`, `packages`, `services`, `trial` and `sync` start every provider (`op`, `brew`, `mise`, `uv`, `dpkg-query`, `launchctl`, `git`) through one bounded runner (`crates/engine/src/proc.rs`), with the rules below.

- **Program lookup.** The program is looked up on a fixed search path: `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, `/bin`, then `~/.local/bin` of HOME. The caller's `PATH` is never used. The entry must be a regular executable file (a link to one counts), owned by root or the current user, and not writable by group or others. Its resolved path must stay under the parent of the search directory that named it: a Homebrew link into its Cellar passes, a link out of the prefix is refused, and for `/bin` the directory itself is the prefix. The first directory that holds the name decides. A failed check there is a refusal, not a fall-through to a later directory. A directory where the name cannot be looked up (permission denied, an I/O error) is also a refusal. Only absence continues the search. The checks apply to the canonical path, examined without following links, and that path is what runs. A `test-isolation` build reads the search path from `DFM_PROVIDER_PATH`.
- **Environment for `op`.** Built from empty: `PATH` (the search path), `HOME`, `LANG=C`, and only the caller's variables whose names start with `OP_SESSION_`, `OP_SERVICE_ACCOUNT_TOKEN` or `OP_CONNECT_`. The desktop app integration needs no variable: `op` finds it under `HOME`.
- **Working directory.** `<cache dir>/run`, created with mode 0700. An existing one must be a real directory owned by the current user, and it is set to 0700 again.
- **Signals.** `secrets`, `packages`, `services` and `trial` install the signal handlers before their first child. `sync` installs none before its first scope runs (docs/sync.md). A stop request (SIGINT, SIGTERM) while a child runs ends the child. A recorded signal then ends the run with exit 130 or 143 before any write. `apply` installs the handlers after the lock, because it starts no child.
- **Process group and bounds.** The child gets its own process group. `op whoami` has 30 s and `op inject` has 120 s. Standard output is bounded at 16 MiB (`E_LIMIT_EXCEEDED`). Standard error is read in chunks, the first 4 KiB are kept in memory and the rest is dropped. It is **never printed or persisted**, because it can repeat a reference or a value. The error names its size and the command to run by hand.
- **Termination.** On the timeout or a stop request, the group gets SIGTERM, a 10 s grace period, then SIGKILL. The runner also waits for the child's pipes to close and for the template to be written. A process that outlives the child and holds its output, or holds its standard input without reading it, gets the same sequence. A run therefore returns within the timeout plus three grace periods. No 30 s wait for the child exists: the child is ended as on a timeout.
- **Not a probe.** `op whoami` and `op inject` talk to the 1Password app or service and are not on the side-effect-free probe allowlist. They run only in `secrets` and in the secrets scope of `sync`. `op whoami` is the one call that `--preview` makes. `sync` runs it in its review, after planning and before it prints the plans, and only for a secrets plan with something other than re-renders (docs/sync.md).

## One operation

The destination is a sensitive output. No unsalted hash of its content exists anywhere. The plan, receipt, journal, `--json`, table and `doctor` show destination and mode only.

1. **Plan time.** The CLI reads the destination in memory (`read_live`: regular, owned by the user, one hard link, at most 16 MiB) and gives the planner a verdict:
   - Absent: `create_file`.
   - Present, and its per-machine salted hash equals the hash the ledger records for this resource: `update_file` ("re-render; the file is written only when the result differs").
   - Present, with no entry of this resource or a different hash: `update_file` with `takeover = true`, which needs `--allow-adopt '<operation-id>'`.
   - Present, with another resource's ledger entry: a blocker (`E_CONFLICT`). Move the file aside.
   - A mode open to group or others, a link, a directory, another owner: a blocker with fix text (`E_CONFLICT`, `E_PRIVILEGE_UNSUPPORTED`).
   - A recorded hash with no `state/salt`: `E_STATE_INVALID` with the store's fix text.

   The plan never says `current`, because the content exists only after the render.
2. **Re-read under the lock**, with the prerequisites and topology checks, as for a merge (`merge_read`). A file that is gone or appeared since the plan is `E_CONFLICT`, blocked.
3. **Ledger check.** Without `takeover`, the live file's salted hash must still equal the ledger's. Otherwise `E_CONFLICT`, blocked, "plan again and approve the takeover".
4. **Template**, read from the source root. Gone, not a plain file, or not the bytes the locked plan was resolved from: `E_PLAN_STALE`, blocked.
5. **Render.** A nonzero exit, a timeout, an empty result, or an `op` that exited before it read the whole template is `E_RENDER_FAILED`. The operation is `failed`, nothing is journaled, and the previous file stays (last-good). The run exits 1. A session that ended after the gate gives the same result, because the provider does not say which it was.
6. **Current.** Rendered bytes equal to the live file, live mode equal to the declared mode, and a matching ledger hash: `current`. Nothing is journaled or written, so a rerun writes no receipt.
7. **Intent, then publish** with the declared mode. A new file is a staged `O_EXCL` file and an exclusive rename. An existing file is replaced with `renameat`, guarded by the identity captured at the re-read (`Publish::ReplaceStamp`). Then the **read-back** (content equal to the bytes in memory, mode equal to the declared mode), the `done` entry, and the **ledger**: class `sensitive`, no fingerprint, `coowned.managed_hash` = the salted hash of the rendered bytes, no entries, no appended items. `state/salt` is created on the first secret write if it does not exist.

The receipt operation has class `sensitive`, no `before` and no `after`, and the destination as its owned output. It has `reload = not_attempted` when the file was written, and an `approvals[]` entry with `how = allow_adopt` for a takeover.

## Decisions

| Question | Choice |
| --- | --- |
| Command shape | A separate `secrets` command with the flags of `apply`, not a flag on `apply`. DESIGN.md section 7 lists `secrets` as the command that authenticates and renders. |
| Where the salted hash lives | `coowned.managed_hash`, the one salted hash the ledger may keep ([docs/state.md](state.md)), with empty `entries`, `appended` and `leaves`. |
| File identity in the ledger | Not recorded: no device, inode, size, mode or mtime beside the salted hash. The salted hash alone decides, and the identity check happens at the rename (`ReplaceStamp`). |
| Overwriting an existing destination | No prompt exists, so `--allow-adopt '<operation-id>'` names it, as for a merge. A hand edit after the manager wrote the file is a takeover too, because the ledger no longer proves the file is the manager's. |
| Provider standard error | Always withheld. A redacted, truncated copy is not kept, because redaction knows only values the manager rendered, and `op` can print a reference path or another value. |
| Credential check | `op whoami --account <account>` exits 0. `op account list` is not enough: it lists a configured, signed-out account, and `op inject` would then wait for a sign-in. |
| Environment variables | `OP_SESSION_*`, `OP_SERVICE_ACCOUNT_TOKEN` and `OP_CONNECT_*` pass through. They are the ways a credential is already available. |

## Known limits

- **Locked vault.** A locked 1Password app or a signed-out account stops the whole `secrets` run at the credential gate (exit 3, nothing rendered). No secret is skipped. In `sync` the secrets scope is skipped instead, with a warning at the end of the output (docs/sync.md).
- **Unlock prompts.** `op whoami` with the desktop app integration can make the app show an unlock prompt. The manager cannot tell a prompt from a slow answer. The 30 s bound ends the call and reports `E_CREDENTIAL_UNAVAILABLE`.
- **Rerun cost.** Every `secrets` run renders every planned secret, so each run talks to the vault once per secret.
- **Rotation under `sync`.** A `dfm sync` with nothing else to change does not contact the vault, so a rotated secret is rendered by the next sync that changes something else or by `dfm secrets` (docs/sync.md).
- **Crash recovery.** As for a merge: a crash after the rename and before the outcome entry leaves an `intent` with no expected hash. Recovery answers `unknown`, and the path blocks until `state resolve <journal-id> --as failed` (docs/apply.md, "Merge crash recovery").
- **Signals.** A stop request during `op inject` ends the child, the operation is `failed`, and nothing is journaled or written.
- **Check-then-act.** A write by another process between the identity check and `renameat` is replaced (docs/apply.md, "Merge check-then-act").
- **Evidence scope.** Tests use the `test-isolation` build, a temporary HOME and an `op` look-alike. No test runs the real `op`.
