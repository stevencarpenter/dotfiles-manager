# Secrets (DFM-22)

Task: DFM-22, the `op-template/v1` adapter and the `secrets` command. This document records what the first secret rendering does and where it resolves or differs from [docs/architecture.md](architecture.md) (the contract) and DESIGN.md section 8. The contract wins where this document is silent. The planner is in [docs/planner.md](planner.md), apply in [docs/apply.md](apply.md), the ledger in [docs/state.md](state.md).

## What exists

| Part | Location |
| --- | --- |
| `Scope` (`config`, `secrets`, `packages`) and the secret decision (`decide_secret`) | `crates/model/src/plan/build.rs` |
| Plan-time verdict on the destination (`secret_facts`), the session check (`credentials`) and the render (`render`) | `crates/cli/src/adapters/secret.rs` |
| The bounded child-process runner (fixed search path, rebuilt environment, process group, timeout, stop request) | `crates/engine/src/proc.rs`, `crates/engine/src/signal.rs` (`kill_child`) |
| `Generator::renders` and `Generator::render` | `crates/engine/src/generate.rs` |
| Execution (`try_render`: re-read, ledger check, template, render, publish, read-back, ledger) | `crates/engine/src/apply.rs` |
| `secrets` command (the `apply` command over the secrets scope) | `crates/cli/src/apply_cmd.rs`, `crates/cli/src/lib.rs` |
| Codes `E_CREDENTIAL_UNAVAILABLE` (3) and `E_RENDER_FAILED` (1) | `crates/model/src/refusal.rs` |
| Tests with an `op` look-alike, and the runner's own tests | `crates/cli/tests/secrets.rs`, `crates/cli/tests/runner.rs` |

`dfm-model` stays pure: the planner takes a verdict and never reads a file. `dfm-engine` starts the child. No dependency was added. `unsafe_code` stays forbidden except in `signal.rs`, which gains one `kill` call.

## The command

```
dfm secrets [--root <dir>] [--preview] [-y|--yes-plan <digest>] [--allow-adopt <operation-id>]...
```

`secrets` is `apply` over the secrets scope. It resolves, observes and plans like `apply`, with one difference in what the plan covers: every `secret` resource is planned, and every other resource is listed as `not_in_scope` ("link is configuration, outside a secrets plan"). The counts still add up to the number of participants. A configuration plan (`plan`, `apply`) keeps listing a secret as `not_in_scope` with destination and mode only, and never reads the destination or starts `op`.

Order of one run:

1. Resolve, observe, plan. The plan prints with its digest. A blocked plan exits 3.
2. The gates that need no write (docs/apply.md), then the credential gate: for each account a planned secret names, `op whoami --account <account>` must exit 0. Otherwise exit 3 with `E_CREDENTIAL_UNAVAILABLE`, before the lock and before any write. An `op` that is missing from the search path or fails its executable checks is `E_PROBE_FAILED`, also exit 3. The preview runs this gate too, so `--preview` exits 0 only when `secrets` would render.
3. Without `--yes-plan`: exit 3, `E_APPROVAL_REQUIRED`. With a digest that differs: `E_PLAN_STALE`.
4. The writer lock, the second plan, settling, then one operation per secret (below). The signal handlers are already in: `secrets` installs them before its first child, the credential gate. Receipt and report as for `apply`, with `command = secrets`.

`--allow-disruptive` is a usage error (exit 2): a secrets plan removes nothing. A deselected secret keeps its file and its ledger entry; a sensitive removal is still refused (docs/apply.md, Delete safety).

## The adapter

`op-template/v1` value: `template` (a source file of the module, relative to the manifest), `account` (the 1Password account: letters, digits, `.`, `_`, `-`, `@`, not starting with `-`, at most 128 characters) and `mode` (an integer file mode with group and other bits clear and owner read kept, since the manager reads the file back after a write and at every plan: `0o600` and `0o400` pass, `0o200` and `0` are `E_MANIFEST_INVALID`). The resolver records no content identity for the template and `explain` prints `[redacted]` for the values (docs/resolver.md item 12); nothing here changes that.

The render is `op inject --account <account>` with the template bytes on standard input and the result on standard output. The template is read from the plan's source root as any source is (below the root without following a link, a regular file of the user with one hard link, at most 1 MiB), and it must be byte for byte the template the locked plan was resolved from: the plan records no content identity for it, so the `secrets` command keeps the bytes it loaded and the render refuses any other (`E_PLAN_STALE`, the operation is blocked, nothing is rendered). No `-i` or `-o` flag is used, so `op` never opens a path the manager did not read, and the result exists only in memory until it is published.

## How `op` is run

The runner (`crates/engine/src/proc.rs`) applies the probe rules of architecture section 8:

- **argv[0].** `op` is looked up on a fixed search path (`/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, `/bin`, then `~/.local/bin` of HOME, where mise's official installer puts `mise`), never on the caller's `PATH`. The entry must be a regular executable file (a link to one counts) owned by root or the current user, not writable by group or others, and its resolved path must stay under the parent of the search directory that named it (a Homebrew link into its Cellar stays; a link out of the prefix is refused; for `/bin` the directory itself is the prefix). The first directory that holds the name decides: a failed check there is refused, not shadowed by a later directory, and so is a directory where the name cannot be looked up (permission denied, an I/O error); only absence continues the search. The checks apply to the canonical path, examined without following links, and that path is what runs; the prefix comes from the canonical search directory, so a search directory reached through a symlink is compared on the same terms. The isolated build reads the search path from `DFM_PROVIDER_PATH`, so tests plant a look-alike.
- **Environment.** Built from empty: `PATH` (the search path), `HOME`, `LANG=C`, and only the caller's variables whose names start with `OP_SESSION_`, `OP_SERVICE_ACCOUNT_TOKEN` or `OP_CONNECT_`. A session that is already available reaches `op`; nothing else of the environment does. The desktop app integration needs no variable: `op` finds it under `HOME`.
- **Working directory.** `<cache dir>/run`: created with mode 0700, and an existing one must be a real directory owned by the current user (it is set to 0700 again).
- **Signals first.** `secrets` installs the manager's signal handlers before its first child (the session check), so a stop request during the session check, a probe or a render ends the child instead of leaving it running. A recorded signal then ends the run with exit 130 or 143 before any write. `apply` still installs them after the lock, because it starts no child.
- **Process group and bounds.** The child gets its own process group. `op whoami` has 30 s, `op inject` 120 s. Standard output is bounded at 16 MiB (`E_LIMIT_EXCEEDED`); standard error is read in chunks, the first 4 KiB are kept in memory and the rest is dropped, and it is **never printed or persisted**: it can repeat a reference or a value. On the timeout, or when a stop request (SIGINT, SIGTERM) arrives while the child runs, the group gets SIGTERM, a 10 s grace period, then SIGKILL (section 10 item 6). The runner waits for the child's pipes to close and for the template to be written, not only for the child: a process that outlives the child while holding its output, or holding its standard input without reading it, gets the same SIGTERM, grace, SIGKILL sequence, and a stop request that arrives while such a process lingers starts that sequence at once, so a run returns within the timeout plus three grace periods whatever the child started. The 30 s wait of the target contract is not built (the child is ended as on a timeout).
- **Not a probe.** `op whoami` and `op inject` talk to the 1Password app or service and are not on the side-effect-free probe allowlist. They run only in `secrets`, after the plan is shown, and `op whoami` is the one call a `--preview` makes.

## One operation

The destination is a sensitive output. As for the co-owned merge, no unsalted hash of its content exists anywhere, and the plan, receipt, journal, `--json`, table and `doctor` show destination and mode only.

1. **Plan time.** The CLI reads the destination in memory (`read_live`: regular, owned by the user, one hard link, at most 16 MiB) and gives the planner a verdict. Absent: `Owned` (a `create_file`). Present and its per-machine salted hash equals the hash the ledger records for this resource: `Owned` (an `update_file`, "re-render; the file is written only when the result differs"). Present with no entry of this resource or a different hash: `Unowned`, planned as `update_file` with `takeover = true`, which needs `--allow-adopt '<operation-id>'`. Present with another resource's ledger entry: a blocker (`E_CONFLICT`), decided before the verdict; move the file aside. A mode open to group or others, a link, a directory, another owner: a blocker with fix text (`E_CONFLICT`, `E_PRIVILEGE_UNSUPPORTED`). A recorded hash with no `state/salt`: `E_STATE_INVALID` with the store's fix text. The plan never says `current`: the content exists only after the render.
2. **Prerequisites, topology, re-read** under the lock, as for a merge (`merge_read`). A file that is gone or appeared since the plan is `E_CONFLICT`, blocked.
3. **Ledger check.** Without `takeover`, the live file's salted hash must still equal the ledger's: else `E_CONFLICT`, blocked, "plan again and approve the takeover".
4. **Template**, read from the source root. Gone, not a plain file, or not the bytes the locked plan was resolved from: `E_PLAN_STALE`, blocked.
5. **Render.** A nonzero exit, a timeout, an empty result, or an `op` that exited before it read the whole template is `E_RENDER_FAILED`: the operation is `failed`, nothing is journaled, the previous file stays (architecture section 8 item 6, last-good). The run exits 1. A session that went away after the gate ends the same way: the provider does not say which it was, and its standard error is withheld.
6. **Current.** Rendered bytes equal to the live file, the live mode equal to the declared mode, and a ledger hash that matches: `current`. Nothing is journaled or written, so a rerun writes no receipt.
7. **Intent**, then **publish** with the declared mode: a staged `O_EXCL` file and an exclusive rename for a new file, or `renameat` guarded by the identity captured at the re-read for an existing one (`Publish::ReplaceStamp`). Then the **read-back** (content equal to the bytes in memory, mode equal to the declared mode), the `done` entry, and the **ledger**: class `sensitive`, no fingerprint, `coowned.managed_hash` = the salted hash of the rendered bytes, no entries, no appended items. `state/salt` is created on the first secret write if it does not exist yet.

The receipt operation has class `sensitive`, no `before` and no `after`, the destination as its owned output, `reload = not_attempted` when the file was written, and an `approvals[]` entry with `how = allow_adopt` for a takeover.

## Decisions where the documents were silent

| Question | Choice |
| --- | --- |
| Command shape | A separate `secrets` command with the flags of `apply`, not a flag on `apply`. DESIGN.md section 7 lists `secrets` as the command that authenticates and renders. |
| Where the salted hash of a secret output lives | `coowned.managed_hash`, the one salted hash the ledger may keep (architecture section 8 item 7), with empty `entries`, `appended` and `leaves`. No ledger schema change. |
| File identity in the ledger | Not recorded. The contract asks for device, inode, size, mode and mtime beside the salted hash; the salted hash alone decides, and the identity check happens at the rename (`ReplaceStamp`). |
| Overwriting an existing destination | The contract asks for an interactive confirmation that names the destination. No prompt exists, so the rule the merge uses applies: `--allow-adopt '<operation-id>'`, which names it. A hand edit after the manager wrote the file is a takeover too, because the ledger no longer proves the file is the manager's. |
| Provider standard error | Withheld, always. The contract allows a redacted, truncated copy; redaction knows only values the manager rendered, and `op` can print a reference path or a value the manager did not render. The error names its size and the command to run by hand. |
| Credential check | `op whoami --account <account>` exits 0. `op account list` is not enough: it lists a configured, signed-out account, and `op inject` would then wait for a sign-in (the owner's `op-render` script records the same finding). |
| Current without a plan-time comparison | The plan always plans a render. `current` is decided after the render, in memory. |
| Deselected secret | Nothing. The configuration plan lists the entry as `current` with the text "kept with its ledger entry" and plans no removal and no retire; `authorizes_delete` refuses every sensitive output. Delete the file by hand if it must go. |
| Environment variables | `OP_SESSION_*`, `OP_SERVICE_ACCOUNT_TOKEN` and `OP_CONNECT_*` pass through. They are the ways a credential is "already available" (architecture section 13, secret variant). |

## Known limits

- **Rerun cost.** Every `secrets` run renders every planned secret, so each run talks to the vault once per secret. There is no plan-time `current`.
- **Crash recovery.** As for a merge: a crash after the rename and before the outcome entry leaves an `intent` with no expected hash, so recovery answers `unknown` and the path blocks until `state resolve <journal-id> --as failed` (docs/apply.md, "Merge crash recovery").
- **Interactive prompts.** `op whoami` with the desktop app integration can make the app show an unlock prompt. The manager cannot tell a prompt from a slow answer; the 30 s bound ends the call and reports `E_CREDENTIAL_UNAVAILABLE`.
- **Signals.** A stop request during `op inject` ends the child (SIGTERM, 10 s, SIGKILL) and the operation is `failed`, with nothing journaled or written. The 30 s wait of the target contract is not built.
- **Check-then-act.** Between the identity check and `renameat` a write by another process is replaced (docs/apply.md, "Merge check-then-act").
- **Evidence scope.** Tests use the `test-isolation` build, a temporary HOME and an `op` look-alike. The real `op` was not run by a test.
