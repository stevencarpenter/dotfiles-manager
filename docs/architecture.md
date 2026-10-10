# Minimum implementation contract

Status: **Proposed; owner review pending.** Task: DFM-10. This document selects the implementation language and fixes the minimum formats and boundaries. It implements nothing. [DESIGN.md](../DESIGN.md) remains the requirements source. This document takes precedence over DESIGN.md section 11 only after the owner accepts the whole document.

Criteria covered: C01 to C06 and C08 to C10. C06 is **prose-validated only**: no resolver, code or device ran. C07 (independent review) and C11 (delivery evidence) remain open.

The owner answered the questions in section 14 on 2026-10-09. Those answers are recorded as decisions. They are inputs to this proposal, not acceptance of the document.

## 1. Decision summary

1. Language: Rust (stable, edition 2024), one binary, no runtime interpreter. The choice is conditional on bootstrap cost (section 2).
2. Dependencies: the Rust standard library plus four direct crates (`serde`, `toml`, `serde_json`, `sha2`) with exact version pins and transitive dependencies locked by `Cargo.lock`. A `cargo-deny` gate and `cargo-audit` or `cargo-vet` run in the local gate. Hand-written argument parsing. No async runtime, plugin loader or expression language.
3. Adapters: compiled into the binary behind typed Rust traits. No marketplace and no dynamic loading.
4. Formats: TOML for authored documents and the source lock. JSON for machine-written state, ledger, receipts and journal. Every document carries an integer `schema`.
5. Distribution: build from source on each machine. No release artifact, signing or notarization in the first delivery. The binary installs into a manager-owned versioned directory selected by a symlink. The coordinator updates itself last.

Scope: `firstmate` applies to the personal M5 only. The work M3 profile does not include it. Adding it to the M3 is deferred and is not a first-delivery requirement.

## 2. Prior-art check and rationale record

Hippo query (`mcp__hippo__ask` and `search_hybrid`, 2026-10-09): no prior record selects an implementation language for this manager. Related history exists: the owner evaluated chezmoi, Nix, Stow, YADM and Dotbot for the current dotfiles (chezmoi needed repeated re-adds for live edits, Nix was heavy for frequent small edits, Stow had no generation), and a chezmoi to nix-darwin port was tried on a branch. Those results motivate a replacement manager. They say nothing about its language. The `ask` call was degraded (inference backend unreachable), so the conclusion rests on `search_hybrid` only.

### Language trade-offs

Verified on the authoring machine (Apple Silicon): `/usr/bin/python3` is 3.9.6 and has no `tomllib`. Python 3.14.7, uv 0.12.17, mise, Rust 1.98.1 and Apple Swift 6.4 are installed. Python, uv and Rust arrive through mise or Nix shims, so the manager itself can update or retire them.

| Option | For | Against |
| --- | --- | --- |
| Python 3.14, standard library, run through uv | Zero third-party code. `tomllib`, `json`, `hashlib`, `os.replace` exist. The repo already uses it for `scripts/check_examples.py`. No compile step. Fastest to write. | Needs a managed interpreter at run time. `update` can retire that interpreter mid-operation. A fresh machine needs uv before anything runs. Weaker static typing at the adapter boundary. |
| Swift | OS-owned toolchain via Command Line Tools. Foundation and CryptoKit. | No TOML in the standard library or Foundation. Foundation differs on Linux. Ties core code to Apple tooling (against C04 intent). |
| Go | Static binary. Rich standard library. | No TOML in the standard library. Weaker enum modelling for typed operations and outcomes. |
| Rust | Static binary: nothing the manager can retire is needed at run time. Enums and traits express operations, outcomes and ports with exhaustive matching. Fixed supply chain through `Cargo.lock`. | No TOML in the standard library, so four direct crates. Slower to write. Every device needs a pinned toolchain and the Apple linker at build time. |

### Condition on the Rust choice

Rust's advantage over Python 3.14 plus uv holds only when one of these is true:

1. A prebuilt artifact is distributed (rejected for first delivery, decision 2), or
2. A pinned Rust toolchain is confirmed present on each target device at bootstrap and at every self-update.

The owner accepted condition 2 for first delivery (decision 1). It is **unverified** on the personal M5 and the work M3 until the device gate in section 13. If the gate fails, the documented alternative is Python 3.14 standard library run through uv, with the interpreter-retirement risk handled by the self-update ordering rule in section 8.

Self-update by build adds a prerequisite: the exact pinned toolchain version must already be installed when `update` builds, with the toolchain tree digest defined in section 8. If a lock change bumps the toolchain pin, installing the new toolchain is a separate earlier operation that needs network access, needs interactive confirmation, and is shown in the plan with its toolchain tree digest. The build step has no network guarantee beyond Cargo's own offline mode (section 8, build containment).

## 3. Toolchain and repository layout (C01)

- `rust-toolchain.toml` pins an exact version (for example `1.98.1`), not a channel. `Cargo.lock` is committed. Direct crates are pinned with `=x.y.z`. The vendor directory is not committed.
- Gate additions, once the workspace exists, in `scripts/check_all.sh`: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test --locked`, `cargo deny check`, and `cargo audit` or `cargo vet`. The shared hook and CI script rule in AGENTS.md is unchanged.
- Workspace crates and permitted dependencies (an arrow means "depends on"):

```
model          -> (nothing)       data and pure logic: documents, schema checks, resolver,
                                  provenance, and the shared data types Typed, Claims,
                                  Operation, Observation, ProbeRequest, Check, Outcome
ports          -> model           traits only: Adapter, Fs, Clock, Runner, Vault, Platform,
                                  Installer, Inspector, Generator, Reloader, UpdateProvider
adapters       -> ports, model    generic adapters: fs-link, git-config, claude-settings,
                                  text-fragments, skills
engine         -> ports, model    planner, applier, ledger, receipts, journal, registry
platform_macos -> ports, model    macOS provider adapters and port implementations
cli            -> engine, adapters, platform_macos   parsing, rendering, exit mapping
```


- `model` takes already-read bytes and returns typed values. It performs no filesystem, process or network call and depends on no other crate. `engine` reads sources through the `Fs` port and passes bytes to `model`.
- Type placement: data types (`Typed`, `Claims`, `Operation`, `Observation`, `Outcome`) live in `model`. Traits (`Adapter` and the section 11 ports) live in `ports`. `adapters` and `platform_macos` implement `Adapter` and the ports. Neither depends on `engine`. `engine` calls adapters only through the `Adapter` trait in `ports`.
- `adapters` is the generic adapter crate. It contains no OS-bound path or tool.
- `platform_macos` is the only crate that names macOS prefixes, macOS-provided tools or APIs, and the provider tools that are OS-bound (Homebrew, launchd, application bundles, `op`, mise). A test fails the gate if those names or prefix literals appear in `model`, `ports`, `engine` or `adapters`.
- The registry maps adapter identifiers to implementations. `cli` builds it. `engine` knows only the trait.
- Release profile and features: `test-isolation` (section 7) is never in `default` features. A gate test fails if a release build graph enables it.

## 4. Versioned schema and compatibility (C01, C03)

Every authored and machine-written document has an integer top-level `schema`. Document kinds: `bundle`, `module`, `profile`, `lock`, `binding`, `state`, `ledger`, `grants`, `receipt`, `journal-entry`.

### Compatibility table

Each kind has three compiled-in lists. Initial values:

| Kind | Writes | Reads | Upgrades from |
| --- | --- | --- | --- |
| `bundle`, `module`, `profile`, `lock`, `binding` (authored or reviewed) | 1 | 1 | none (never auto-upgraded) |
| `state`, `ledger`, `grants` (machine owned) | 1 | 1 | none yet |
| `receipt`, `journal-entry` | 1 | 1 | none yet (read-only readers per old version) |

### Rules

1. **Refusal applies to versions outside (Reads union Upgrades from).** A version outside that set is refused with `E_SCHEMA_UNSUPPORTED` before any other field is read. The error names the file, found version and supported lists.
2. A version in "Upgrades from" but not in "Reads" is converted by the upgrade procedure below. Authored kinds have an empty upgrade list: a schema migration of authored files is an explicit, reviewed edit.
3. Schema 1 accepts known keys only (DESIGN.md section 3). Any added key requires a new schema version. The manager never ignores an unknown key.
4. The binary writes only the newest version of each kind. Release notes state the new "Writes", "Reads" and "Upgrades from" lists for each kind.

### Where state lives and who owns the ledger (F1)

State root: `~/.local/state/dotfiles-manager/` (directories 0700, files 0600).

| Path | Content | Mutability |
| --- | --- | --- |
| `state/ledger.json` | The ownership ledger: for each resource, the exact `owned_outputs` the manager may later remove, with fingerprints and the `op_id` that last wrote them. | Rewritten atomically after each operation reaches a terminal journal entry. |
| `state/receipts/` | One receipt per run (history and evidence). | Immutable. Never rewritten or converted. |
| `state/grants.json` | Origin-trust grants: one entry per canonical origin with the time, how it was granted (`interactive`) and the grant id. Converted by the state upgrade like the ledger. | Rewritten atomically, only by an interactive grant. |
| `state/journal.jsonl` | Append-only intent and outcome entries. | Append only. |
| `state/snapshots/` | Restorable copies for adapters that support restore (section 8). | Written per operation, pruned by retention. |
| `state/trials/` | Active trial records and baselines. | Rewritten atomically. |

The **ledger is the only source of delete authority.** A receipt never grants it. Consequences:

1. The ledger is part of `state` and is converted by the state upgrade. Because it is self-contained, an unreadable or unsupported old receipt cannot change delete authority in either direction.
2. Receipts stay in their original schema. One read-only reader per supported old receipt version converts to the current in-memory type for display and `doctor`. A receipt outside Reads union Upgrades from is shown as `unreadable` and affects nothing else.
3. A ledger entry whose fingerprint no longer matches the live output is a conflict. It authorizes no deletion.
4. Loss or corruption of the ledger makes all outputs unowned (state-loss rule in DESIGN.md section 7). Recovery is inspect-and-adopt or restore from a snapshot, never forced apply.

### State upgrade and recovery

1. The upgrade holds the lock (section 10) taken on the parent directory, so the lock is not inside the renamed directory.
2. It reads `state/`, writes the new format into `state.next/`, validates it, writes `state.next/COMPLETE`, then renames `state` to `state.prev-<n>` and `state.next` to `state`. `<n>` is the upgrade id, an integer one greater than the highest existing `state.prev-*`, so names never collide. The new `state` gets a record `state/UPGRADE` with `upgrade_id` and the digest of the converted state. The order of the two renames is fixed.
3. Recovery by directory shape: `state.next/` without `COMPLETE` is discarded and the upgrade restarts from the untouched `state/`. `state.next/COMPLETE` with no `state/` completes the rename. `state/` with `state.prev-*` is the normal post-upgrade shape.
4. State newer than the binary supports, or outside Reads union Upgrades from, is refused with `E_STATE_NEWER` or `E_SCHEMA_UNSUPPORTED`. There is no downgrade. Recovery is `state.prev-<n>` plus an older binary version kept by the rule in section 12.

### Rollback window (F3, N2, N3)

1. After an upgrade, `state.prev-<n>` and the previous binary version directory are kept.
2. `doctor` stays read-only. It writes no marker. It reports whether the upgraded state is verified.
3. The explicit command `verify --mark` takes the lock, checks the state, ledger and journal, and writes `state/VERIFIED` with `O_EXCL`. The marker holds the `upgrade_id` and a digest of `ledger.json` and `UPGRADE` at marking time. The command refuses if the marker exists, if any check fails, or if no upgrade is pending verification. An upgrade never copies a marker forward: `state.next/` is created without one.
4. Removing `state.prev-<n>` and the previous binary directory is a planned, journaled operation (`retire-previous`). It appears as a preview line, is part of the plan digest and has its own journal entries. The first successful mutating run plans it only when the marker's `upgrade_id` equals the current `UPGRADE` record and its digest equals the digest computed at plan time. A mutating run that changes the ledger in between makes the marker stale, and `verify --mark` must run again.
5. If the marker never appears or is stale, `state.prev-<n>` stays. A failed, interrupted or read-only run never removes it.

## 5. Document formats (C01)

Authored formats are the existing fixtures: `bundle.toml`, `module.toml`, `profile`. Nothing here changes them.

### Parser and read limits (S10)

1. Authored TOML and the lock: at most 1 MiB per file, nesting depth at most 32, at most 10,000 resources in the resolved composition, string values at most 64 KiB.
2. JSON state files: at most 16 MiB per file, depth at most 32. Journal lines at most 64 KiB. A line over the limit is a journal error, not a skipped line.
3. All reads of authored, state or source files are bounded to the limit of their kind. A file over the limit is refused with `E_LIMIT_EXCEEDED` without being fully read. Source files for adapters have a per-adapter limit.

### Source lock

`dotfiles-manager.lock` is TOML, kept in the source that owns the pin (never private module details in the public core). The manager writes it only during `update` or `sync` after approval, as a reviewable local edit with no automatic commit.

```toml
schema = 1

[[sources]]
id = "example-external"
kind = "git"                      # "git" or "path"
origin = "https://example.invalid/org/repo.git"  # canonical form, section 8
ref = "refs/heads/main"
commit = "<40 hex>"               # must be reachable from ref
tree = "sha256:<64 hex>"          # recomputed digest, section 8

[[adapters]]
id = "homebrew/v1"
required_by = ["personal-dev.atuin"]

[manager]                         # present only when the manager is a managed component
version = "<semver>"
binary_sha256 = "<64 hex>"        # record only; never compared across machines
toolchain = "1.98.1"
toolchain_manifest_sha256 = "<64 hex>"   # channel manifest digest, verified at install time (section 8)
toolchain_tree_sha256 = "<64 hex>"       # toolchain tree digest of the installed tree (section 8)
```

For `kind = "path"` (local editable source) the lock stores no commit. Any change to such a source invalidates an existing plan. The lock holds no secret values and no hash of secret material.

### Receipt

JSON, one file per run: `receipts/<utc-timestamp>-<op-id>.json`, canonical form (UTF-8, sorted keys, no insignificant whitespace).

```
schema, op_id, command, started_at, finished_at,
binary{version, commit, sha256},
profile{id, binding_digest}, machine_id,
plan_digest, approval{kind, plan_digest, at},      # kind: "interactive" | "yes_plan" (S11)
sources[{id, commit, tree}],
operations[{id, resource, adapter, class, status, started_at, finished_at,
            before{fingerprint}, after{fingerprint},   # omitted for sensitive operations
            owned_outputs[], error?}],
result{outcome, exit_code, counts}
```

Read-only commands (`plan`, `explain`, `doctor`, `status`, `self-check`, and any `--preview`) write no receipt. `verify --mark` writes only its marker and a journal entry. Every receipt records `approval.kind` as `interactive` or `yes_plan` (S11). It also records `approvals[]` entries for each disruptive operation approved (`{operation_id, how: "interactive" | "allow_disruptive", at}`) and `origin_grants[]` for each origin-trust grant made in that run (`{origin, grant_id, binding_digest_before, binding_digest_after, written_by: "manager", binary_sha256}`). Receipts are **audit evidence only**. They never decide trust. Trust is decided by `state/grants.json`.

`binary.sha256` is recorded for audit only. It is never compared across machines.

**One status vocabulary (F6).** Operation `status` and `counts` keys are the same set. `counts` has one key per status, and the sum of all counts equals the number of participants (every resource or component the plan covers, whether or not it changed).

| Status | Meaning | Journal phase |
| --- | --- | --- |
| `updated` | Operation ran and its postcondition passed. | `done` |
| `current` | `already_done` was true or desired equals observed. | `done` |
| `held` | A declared hold applies, with its reason. | `done` |
| `pending` | Request accepted, completion not yet observed (for example a queued native update). | `pending` (carries the completion token) |
| `failed` | Operation or postcondition failed. | `failed` |
| `blocked` | A prerequisite failed, or a dependency `failed` or `blocked`. | `blocked` |
| `blocked_pending` | A dependency is `pending`, so this operation did not start. Reported separately from `blocked`. | `blocked` (with reason `pending_dependency`) |
| `not_started` | Run stopped before this operation. | none |
| `not_in_scope` | The command's scope excludes it (for example secrets during `sync`). | none |
| `unknown` | A required observation is `UNKNOWN`, so the engine cannot decide. Before any mutation it gives exit 3. After a mutation began it gives exit 1. | `unknown` |

### Journal and interruption records

`journal.jsonl` is append-only, one JSON object per line, `fsync` after each append. Before a mutating operation the engine appends phase `intent` with a digest of its preconditions. After it, the engine appends exactly one of `done`, `failed`, `blocked`, `pending` or `unknown`. An `intent` without a following terminal entry is an interruption record. The next invocation reports it, runs the operation's `already_done` probe, and then writes `done` or `failed`, or writes `unknown` and blocks dependent work. Interruption never authorizes deletion.

### Binding

`~/.config/dotfiles-manager/machine.toml` (DESIGN.md section 3): `schema`, canonical profile path, machine identity, expected platform and architecture, and the declared origins (section 8). Declaring an origin in the binding does not trust it. One binding per machine.

## 6. Authority model in code (C02)

DESIGN.md sections 2, 3 and 5 define the rules. This section records how the implementation holds them.

1. **One bound profile.** `BoundProfile` is constructed only from the binding file. Mutating commands have no `--profile` flag.
2. **Protected core.** The resolver marks every core path not listed in `policy.defaults` or `policy.extensions` as protected. A change to a protected path returns `E_OVERRIDE_FORBIDDEN`. Core cannot name a non-core module in `requires`.
3. **Explicit defaults and contributions.** Only the two policy lists admit change. An override or contribution is a typed `Change { resource, path, value, reason, origin }`. `model` has no generic merge function.
4. **Provenance.** Every effective value carries `Provenance { owner, original, change?, changer, reason, source_file, content_id }`. `explain` prints it. Secret-bearing values hold references only.
5. **Private-to-public direction.** A bundle can `extends` exactly one ancestor and cannot be referenced by it. The resolver rejects a public source whose closure reaches an external source. Module paths resolve against the declaring manifest and stay inside its trusted root.
6. **Order independence.** The resolver sorts only for display (core first, then lexical). Conflict detection runs on sets.

## 7. OS separation (C04)

1. `model`, `ports`, `engine` and `adapters` contain no prefix literal, no model name and no OS-bound tool name. A gate test enforces this (section 3).
2. HOME is derived once by `platform_macos`: the password-database home of the effective user, which must equal `$HOME`. A mismatch is `E_HOME_MISMATCH` and fails before mutation.
3. **Isolation overrides (F8, N9).** `DFM_HOME`, `DFM_BINDING` and `--home` exist only in a build with the cargo feature `test-isolation`. Release builds do not compile them: the environment variables are ignored and `--home` is an unknown flag (exit 2). A gate test checks that a release build never enables `test-isolation`. In a `test-isolation` build, every report header and JSON output show `isolated: true` and the override values. `self-check` takes an explicit `--fixture-dir <path>` argument in every build, reads only that directory, and makes no network call.
4. Provider locations are observations, not constants. Each provider adapter declares an ordered candidate search and verifies the result (exists, executable, trusted owner and mode per section 8). The verified location is recorded in the report and receipt.
5. The `Platform` port exposes `home()`, `case_fold(path)`, `locate(provider)`, `runner()`, `build_dir()` and `cache_dir()`. Composition, planning and state use only the port.
6. Optional GUI or service resources exist only in modules a profile selects. The engine has no GUI assumption.
7. Not built: Linux providers, Intel-specific paths, portability layers. The only commitment is that nothing in `model` or `engine` would change to add them.

## 8. Trust, probes, privilege, snapshots, secrets (C05)

### Source identity and fetch

A source identity is `(canonical origin, resolved commit, tree digest)`.

**Canonical origin (S2).** Exactly one form: scheme `https` or `ssh`; host lowercase ASCII (IDNA converted); no userinfo except the fixed `git` user for `ssh`; no port unless non-default; path without trailing `/` and without a `.git` suffix; no query or fragment. Anything that does not canonicalize to this form is `E_SOURCE_UNTRUSTED`. Comparison is exact string equality of the canonical form.

**Fetch environment (S2, N4).** The manager builds the Git environment from scratch, as for probes: it starts empty and adds only `PATH` (fixed), `HOME` (a manager-owned empty directory), `LANG=C`, `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_SYSTEM=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`, `GIT_TERMINAL_PROMPT=0`, `GIT_ALLOW_PROTOCOL=https:ssh`, and the one credential helper declared in the binding. It never inherits `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_*`, `GIT_CONFIG_PARAMETERS`, `GIT_SSH_COMMAND`, `GIT_PROXY_COMMAND`, `GIT_ASKPASS`, `HTTPS_PROXY` or any other variable from the caller. The git binary path is pinned and verified by the argv[0] rule in the probe section. Command-line settings: `protocol.allow=never` with `https` and `ssh` enabled, `core.hooksPath=/dev/null`, `core.fsmonitor=false`, `transfer.fsckObjects=true`, no `url.*.insteadOf`. Redirects to a different canonical origin fail. The `ssh` transport still reads `~/.ssh` (keys, `known_hosts`, `config`) through the password database home, so substituting HOME does not isolate it. The host-key policy for `ssh` origins is a device-gate item (section 13). Fetch runs into a staging directory and never changes a linked checkout.

**Tree digest (S1).** The manager recomputes the digest from the checked-out staging tree. It never trusts a digest supplied by the source.

1. The digest is SHA-256 over sorted entries `<relative path> NUL <mode class> NUL <content sha256 or symlink target>`. Mode class is `file` or `exec` (executable bit), or `symlink`.
2. Entries allowed: regular files and symlinks. Submodules, gitlinks, devices, sockets, FIFOs and any non-regular entry are rejected with `E_SOURCE_UNTRUSTED`.
3. A symlink target must stay inside the source root; otherwise `E_SOURCE_ESCAPE`.
4. The pinned commit must be reachable from the declared `ref` in the fetched history.
5. Any lock change is shown with the old and new commit, the old and new digest, and a diff stat (`git diff --stat old..new`) in the plan. A lock change without that display is not approvable.

**Trust grant.**

1. **Trust is stored in `state/grants.json`, keyed by canonical origin.** The binding only declares origins. An origin is trusted only if its canonical form has a grant entry. **The grant check applies to every canonical Git origin the manager fetches, whichever source names it: the binding, a lock `origin` entry, or a module-declared source.** It runs before the fetch. Other network sources are not Git origins. Crates.io is covered by the lock source check on `Cargo.lock` (section 8, build). `static.rust-lang.org` is covered by the toolchain-install gate and the manifest digest check. A grant is created only by an interactive approval during `sync` or `update`, **before any network fetch to that origin**. **`--yes-plan` can never grant a new origin (S3, F9).** A noninteractive run, or a `--preview`, that meets an untrusted origin exits 3 with no fetch. An unrelated binding edit does not revoke a grant, because the match key is the canonical origin and not the binding digest. Removing an origin from the binding and re-adding it later does not need a new grant while the entry stays in `grants.json`; revoking is an explicit interactive action that deletes the entry. Receipts record grants as audit evidence and do not decide trust. A binding that names an origin with no grant entry is untrusted (`E_SOURCE_UNTRUSTED`), and `doctor` reports `ORIGIN_UNTRUSTED`.
2. Fetching and resolving never run repository code.
3. **Known exception (F9).** The self-update and bootstrap builds run the fetched workspace's `build.rs` files, procedural macros and the dependencies' build scripts. Trust for that code is the origin plus the recomputed tree digest, and nothing more. This is a documented, accepted risk with no sandbox: a compromised trusted origin can run code as the user at build time. Containment claims are in the self-update section.
4. Commit signatures are not required now (decision 4). Signed commits can be added later by a schema change.

### Read-only probe allowlist

A probe is data owned by an adapter: `{ argv0, argv template, env, timeout, max_output, expects }`.

1. Allowed without an entry: `lstat`, `readlink`, directory listing and bounded reads of declared paths, all through the `Fs` port.
2. Never allowed: running a discovered or shimmed binary for `--version`, any network call, vault call, install, update or repair. **Bootstrap and self-update follow the same rule:** no `cargo`, `rustc` or `rustup` process starts before the toolchain tree digest check and the build controls pass. The toolchain lives at a fixed manager-owned path (`<manager data dir>/toolchains/<version>/`). The version is read from the path and verified by the manifest and tree digest, never by executing a tool.
3. **Parameters (S6).** Each substituted parameter must match an adapter-declared regular expression (for example `^[a-z0-9][a-z0-9._+@-]{0,63}$`). A value starting with `-` is rejected. The template places `--` before positional parameters when the tool supports it.
4. **argv[0] (S6).** An absolute path, verified by the `Fs` port: owned by root or the current user, not group- or world-writable, not inside a HOME-writable directory used for shims, and not a symlink chain leaving a trusted prefix. The verification result is part of the observation.
5. **Process context (S6).** The runner starts the process in a fixed manager-owned empty directory as cwd, with a rebuilt environment (`PATH` fixed by the provider, `HOME`, `LANG=C`) plus adapter variables. Homebrew probes disable auto-update and analytics. mise probes disable config discovery and auto-install. Each of those settings is part of the test for that probe.
6. **Re-verification (S6).** A probe's recorded behavior is tied to the tool version it was tested with. When the observed tool version changes, the probe result is `UNKNOWN` until its fixture test is rerun against that version.
7. **The control is the fixture-backed tests.** Runtime enforcement is the exact argv match and the checks above. The evidence that a probe has no write and no network effect is its test, run against a process-level fake and, at integration, the real tool. An untested probe stays disabled and returns `UNKNOWN`.
8. Candidate entries to verify in the implementation phase (not verified here): a package installed-version listing, a runtime-tool installed listing, a service listing, reading an application bundle's metadata file.
9. A timeout, a non-allowlisted request or an unsupported fact yields `UNKNOWN` with a reason. Unknown support fails closed.

### Prerequisites and postconditions

Every typed operation declares both as data the engine checks:

- Prerequisites: observation fingerprints that must still hold at apply time, source identities, privilege class, credential availability, quiescence requirement.
- Postcondition: a probe or file check that proves the effect. A correct file, an installed executable and a reloaded consumer are three separate postconditions.
- A failed prerequisite yields `blocked`. A failed postcondition yields `failed`. An `UNKNOWN` required observation yields `unknown`. None is ever reported as success.
- **The engine re-verifies each operation's prerequisites immediately before that operation applies (S3), even when the plan digest matched.**

### Privilege boundaries

1. Privilege classes: `user` (default), `privileged` (explicit scope, separate adapter), `native-ui` (needs the application's own prompt).
2. HOME adapters never escalate.
3. **Privileged operations (S9).** Each is an exact argv vector, validated against an adapter-declared template, executed directly without a shell. The argv is part of the plan digest and is shown in the plan. The manager never persists sudo timestamps or credentials, never changes the sudo timestamp policy, and strips secret-bearing environment variables (all variables added for vault or credential use) from the child's environment.
4. A noninteractive run checks privilege availability without a prompt before mutation. If unavailable, the run exits 3.

### Filesystem writes (S8)

Writes that publish into a target directory use descriptor-relative operations: open the verified parent with `O_DIRECTORY | O_NOFOLLOW` and then `openat`, `renameat`, `symlinkat`, `unlinkat` and `fstatat` with `AT_SYMLINK_NOFOLLOW` against that descriptor. The parent chain is validated component by component with the same flags. The rule closes the window between "check parents" and "write". A platform without these calls is unsupported for that adapter and the adapter fails closed. Atomic publication is: same-directory temporary file created with `O_EXCL`, validated, then `renameat`.

### Secrets and sensitive paths (S7)

1. Resources have a sensitivity class: `normal` or `sensitive`. Secret outputs are always `sensitive`. A co-owned file may be declared `sensitive` by its adapter when it can hold credentials.
2. For `sensitive` paths the manager stores no snapshot content, no unsalted content hash and no diff in the ledger, receipts, journal, snapshots or `--json`. It stores the destination, mode and an owner label.
3. **Redaction rule for every persisted or printed field** (receipt `error`, journal entries, `--json`, table, provider stderr): a value is replaced by `[redacted]` if it came from a `sensitive` path, a secret provider, an environment variable declared secret, or matches a credential pattern list. Provider stderr is captured, truncated to 4 KiB, redacted, then stored.
4. **Test (required).** A fake provider writes a planted secret string into stderr, stdout, an error message and a file name. The test asserts the string is absent from every persisted file and every output format. The test also asserts that the salted hash of a sensitive path appears only in `ledger.json`: not in receipts, journal, snapshots, preview output, `--json`, logs, the default table, `doctor` output or `explain` output.
5. Snapshots are `normal` class only. `claude-settings/v1` is class `sensitive` because the live file can hold credentials in foreign fields. It has no snapshot content, and its fingerprint covers managed fields only. No whole-file hash appears in the ledger or in receipts. The adapter preserves foreign fields by re-reading the live file at apply time, and restoring managed fields is done by re-rendering the desired state, not by a snapshot. Snapshots for `normal` class outputs live in `state/snapshots/<op-id>/` (0700 and 0600). Retention: snapshots are kept while the owning receipt's operation is the latest for that resource, and removed on the next successful run after a newer snapshot exists for the same resource, or 30 days after the resource leaves the ledger, whichever is later. Deleting a snapshot is never a side effect of purge. `state.prev-<n>` retention is in section 4.
6. Secret destinations: the new content goes to a same-directory 0600 temporary file and replaces the destination only after a complete, non-empty render. A failed or empty render leaves the previous file untouched.
7. **Conflict detection for sensitive paths (N7).** The manager must not overwrite a user edit silently. For a `sensitive` co-owned file, the ledger stores a per-machine salted hash of the **managed fields only** (the salt is a random 32-byte value in `state`, mode 0600). For a secret output, file identity and metadata (device, inode, size, mode, mtime) are recorded, but **metadata alone can miss an edit** (same size, restored mtime, in-place write). So the ledger also stores a salted hash of the rendered managed content. This is a deliberate, narrow exception to the no-content-hash rule in item 2: the salted hash lives only in `ledger.json` (mode 0600), is never written to receipts, journal, snapshots, `--json`, logs or exports, and the salt never leaves `state` and survives state upgrade (it is copied into `state.next/` unchanged). Receipt `before` and `after` fingerprints are omitted for sensitive operations. A sensitive observation fingerprint **is** the salted hash. Only digests derived from it (for example the plan digest and journal precondition digests) reach the plan digest and the journal. The salted hash itself is never printed in preview, `--json`, the table, `doctor`, `explain` or logs. Anyone who can read `state` as this user can already read the secret output itself. At apply time a mismatch with the ledger entry, or a missing ledger entry for an existing destination, makes the operation `blocked` with `E_CONFLICT`. Overwriting needs an interactive confirmation that names the destination.

### Coordinator self-update last

1. The manager package is a managed component with one extra ordering rule: it is the final operation, with edges from every other operation.
2. Before it runs, every prior operation has a terminal journal entry and the ledger and receipt for them are written and synced.
3. **Build (S5, N4, N5).** The build runs from the staged tree whose digest matched the lock. **Cargo execution model:** the pinned `cargo` is the toolchain's own `bin/cargo` inside the digest-verified toolchain tree, run by absolute path. The build environment is **constructed from empty**, not filtered from the caller's. `RUSTC` is that tree's `rustc`. `PATH` is that tree's `bin` followed by fixed system directories supplied by the `Platform` port, and never `~/.cargo/bin`. The caller's `RUSTUP_TOOLCHAIN` and `RUSTUP_HOME` are not inherited: `RUSTUP_HOME` is a manager-owned empty directory (`<manager data dir>/rustup-home/`, separate from the toolchain tree at `<manager data dir>/toolchains/<version>/`) and `RUSTUP_TOOLCHAIN` is not set. Rustup is not used to resolve the build, so no `rust-toolchain` resolution and no auto-install happens. The toolchain tree digest does not cover rustup metadata, because rustup is not used for the build. **Staged tree checks (reject with `E_SOURCE_UNTRUSTED`):** a `.cargo/config.toml` or legacy `.cargo/config` at any level (a bare `.cargo/` directory does not block); a legacy `rust-toolchain` file; a `rust-toolchain.toml` that differs from the lock's `[manager].toolchain` pin; any `Cargo.toml` in the tree (every one, including workspace members and the members and `exclude` glob matches) with a path dependency that is absolute or resolves outside the staged root; any `[patch.*]` or `[replace]` entry whose source is neither crates.io nor a path inside the staged root. `Cargo.lock` is not part of the path rule, because it records no paths. `CARGO_HOME` is a manager-owned directory created empty for the build, with no `config.toml`. It is never the user's `~/.cargo`. **Cargo discovery:** Cargo reads `.cargo/config.toml` (and the legacy `.cargo/config`) from the working directory and every ancestor directory. The build runs in `Platform::build_dir()`, a manager-owned directory under `Platform::cache_dir()`. The build directory is the working directory of the build, and `--manifest-path` does not change Cargo's configuration discovery. The manager refuses (`E_BUILD_CONFIG`) if the build directory itself or any ancestor of it, up to the filesystem root and not just up to HOME, contains `.cargo/config.toml`, a legacy `.cargo/config`, `rust-toolchain` or `rust-toolchain.toml`. A bare `.cargo/` directory, for example from a default rustup install, does not block. When no valid build location exists, the build is refused with `E_BUILD_CONFIG`. There is no fallback. **Timing:** the ancestor check runs at plan time, with exit 3, whenever the toolchain and build directory are known. A failure at build time after that (for example a config that appeared in between) is exit 1. This discovery behavior is **unverified**, owned by gate item DG-1 (section 13), and must be confirmed against the pinned Cargo. **Gate tests (required):** (a) a config with a source replacement planted in the parent and in the grandparent of the build directory; (b) an ancestor `rust-toolchain.toml`; (c) a path dependency that escapes the staged root; (d) a config planted in the build directory itself; (e) a rustup proxy planted in the caller's `PATH` and a `RUSTUP_TOOLCHAIN` value in the caller's environment, where neither may be invoked or read. Expected results: `E_BUILD_CONFIG` for (a), (b) and (d), `E_SOURCE_UNTRUSTED` for (c), the build succeeding without touching the planted proxy or variable for (e), and in no case a crate resolved through a replacement. Before `cargo fetch`, an on-device check parses the staged `Cargo.lock` and requires every package source to be the crates.io registry (workspace members have no source). `cargo fetch --locked` uses crates.io only (no git dependencies, no alternate registries), then `cargo build --release --locked --offline` runs. The rest of the environment is scrubbed in the same way as for probes (fixed `PATH`, a manager-owned `HOME` for the build, no secrets). **Containment limits:** `--offline` and Cargo's offline mode only stop Cargo's own fetches. They do not stop a build script or procedural macro from using the network, and they do not hide the real HOME or the rest of the filesystem. The manager does not claim network isolation or HOME isolation for the build. Bootstrap builds apply the same named controls (section 12, item 3.4), not just the same environment.
3a. **`self-check --build-controls <staged tree>`.** This is the single implementation of the section 8 build controls inside the manager. It is read-only, makes no network call and starts no `cargo`, `rustc` or `rustup` process. It checks the toolchain tree digest, the staged-tree rules, the path-dependency rule, the `Cargo.lock` registry check and the ancestor check, and prints each failed control. Exit 0 means all controls pass. Exit 3 means refused, with `E_BUILD_CONFIG` (configuration or toolchain-file inputs, ancestors, build directory) or `E_SOURCE_UNTRUSTED` (staged tree content). The bootstrap script's checker must give the same results on the same inputs (gate tests).
4. The built binary's SHA-256 is recorded in the receipt and in the lock `[manager]` table as a record only. It is never compared across machines (builds need not be reproducible).
4a. **Toolchain provenance (N1).** The toolchain source is the official Rust release distribution (`static.rust-lang.org`). The lock pins the exact version, the SHA-256 of the channel manifest, and the **toolchain tree digest**. The toolchain tree digest is computed over the installed toolchain directory tree with the tree-digest rules above (regular files and symlinks only, modes, symlink targets inside the root, non-regular entries rejected). The lock value comes from a reviewed install and is shown in the lock diff. At install time the manager verifies the channel manifest digest before it installs anything. Before every build, the manager recomputes the toolchain tree digest of the installed toolchain, and a mismatch is `E_TOOLCHAIN_MISMATCH`. Installing a toolchain is a typed operation (`toolchain-install`) shown in the plan with network class `network`, the manifest digest and the toolchain tree digest. **It is an interactive-confirmation gate, like new-origin trust.** `--yes-plan` never waives it. A noninteractive run that would change the toolchain exits 3, and so does `--preview`. The manager never relies on rustup's toolchain resolution (Cargo execution model, item 3). The staged `rust-toolchain.toml` must equal the lock pin (item 3).
4b. **Audit gates (N5).** `cargo deny` and `cargo audit` or `cargo vet` run at development time and in the CI gate for the commit that is locked. They do not run on the device. The lock pins a commit that passed the gate, and the tree digest binds the content.
5. **Self-check (S5, N9).** The new binary runs `self-check --fixture-dir <temporary copy of binding and state>` with a scrubbed environment. `self-check` is read-only and makes no network call. On success the active symlink flips in one atomic rename.
6. The running process keeps its old inode and exits after writing its result. A failed self-check leaves the previous version active and records `failed` for that operation only.
7. The previous version directory follows the rollback window in section 4.

## 9. Operation lifecycle and idempotence

The stages are DESIGN.md section 6 (resolve, inspect, plan, apply, verify). Additions:

1. **Plan digest (S3, N8).** SHA-256 over the canonical JSON of the typed plan. The plan includes: the command and its scope, the manager binary identity (version and SHA-256), binding digest, machine identity, resolved model digest, source identities, the toolchain tree digest when a build is planned, observation fingerprints, the ordered operation list with each operation's prerequisites, privilege class and exact privilege argv, the set of operations marked for `--allow-disruptive`, and content hashes of generated outputs for `normal` class outputs. For `sensitive` outputs it includes destination and mode only. **Path-source identity:** for a `kind = "path"` source, the identity is the canonical absolute real path plus the tree digest computed with the same rules as a Git tree (regular files and symlinks only, symlink targets inside the root), recomputed at plan time and again before apply.
2. Each operation implements `already_done(observation) -> bool`. After an interruption the engine uses it to avoid repeating a completed side effect.
3. On the first failure, dependents become `blocked` and operations that have not started become `not_started`. The default is to stop. A `--continue` option is not in the minimum contract.
4. The receipt is written even on failure and on interruption by signal.
5. **Display integrity (N3).** Every untrusted string rendered to a terminal (file names, commit messages, provider output, module text) has control characters and escape sequences replaced by visible escapes. The plan shows every field that changes an effect: target, mode, owner, argv, source, version, privilege class, disruptive flag. Git diffs and diff stats run with `--no-ext-diff --no-textconv` and with the isolated environment.

## 10. CLI, exit semantics, noninteractive behavior (C01, C08)

### Exit codes (F5)

| Code | Name | Meaning |
| --- | --- | --- |
| 0 | `ok` | Completed, including no work needed. |
| 1 | `failed` | Any operation `failed`, `blocked` or `unknown` **after** some mutation began; the run limit expired (`E_RUN_LIMIT`); diagnosed errors or incomplete required checks (also `doctor`). |
| 2 | `usage` | Invalid invocation, or inability to produce the requested report. |
| 3 | `blocked` | A gate failed **before any mutation**: stale plan, missing credential, no TTY for a required prompt, privilege unavailable, untrusted or new origin, or a toolchain change, in a noninteractive run or in `--preview` (before any fetch or install), unsupported version, a required prerequisite that is `UNKNOWN`, lock held. |
| 4 | `declined` | Approval refused or prompt timeout. No mutation. |
| 5 | `pending` | No operation `failed`, `blocked` or `unknown`, and at least one participant is `pending` (including a `pending` operation that left a dependent `blocked_pending`). Not success. |
| 130 | `interrupted` | Stopped by SIGINT after the receipt was written. |
| 143 | `terminated` | Stopped by SIGTERM after the receipt was written. |

**Mixed results.** A run returns the highest-precedence code that applies:

`143 or 130 (signal, first received)  >  2  >  1  >  5  >  3  >  4  >  0`

Codes 3 and 4 mean no mutation happened, so they cannot occur together with 1 or 5 in one run. **Unknown:** a required `UNKNOWN` prerequisite found before any mutation gives 3. One found after a mutation began makes that operation `unknown` and the run exit 1. **Blocked:** an operation that blocks after some mutation is status `blocked` and the run exits 1, not 3. **Pending:** with no failure, a `pending` operation gives 5, even if it left dependents as `blocked_pending`. The receipt counts `pending` and `blocked_pending` separately from `blocked`. Any `failed`, `blocked` or `unknown` outranks `pending`, so the run exits 1.

**`doctor` and exit-3 inputs.** `doctor` never exits 3 or 4. For malformed manifests, unsupported schema or state version, missing modules or an untrusted source, it reports each as a finding with its `E_...` code, inspects whatever is still safe to inspect, and exits 1. It exits 2 only if it cannot produce any report. This keeps DESIGN.md's rule that independent rows stay visible.

Every non-zero result prints one stable code and the first failing location. `--json` repeats the code, exit name, and per-status counts, with the same semantics as the table. Resolver codes are the `expected_error` values in `examples/cases.toml`.

### Commands and flags

DESIGN.md section 7 defines the commands, including `try`, `adopt` and `purge` (all mutating, so they use the plan, preview, approval and receipt rules here). This contract adds `verify --mark` (section 4) `self-check --fixture-dir <path>` and `self-check --build-controls <staged tree>` (section 8). Global flags: `--json`, `--no-input`, `--preview`, `--yes-plan <digest>`, `--allow-disruptive <operation-id>`, `--timeout <duration>`. The isolation overrides in section 7 exist only in `test-isolation` builds. No flag selects a profile on a mutating command.

### Bounded noninteractive behavior

1. Interactive means stdin and stderr are TTYs and `--no-input` is absent. Otherwise the run is noninteractive.
2. Before any mutation the engine computes every gate the plan needs (approval, new-origin trust, toolchain install, credential, privilege, disruptive action). An unsatisfied gate in a noninteractive run ends the run with exit 3 and the list of gates. It never waits.
3. Interactive prompts wait at most 300 s. Expiry exits 4.
4. **Preview.** `sync --preview`, `update --preview`, `try --preview`, `adopt --preview` and `purge --preview` resolve, inspect and plan, print the plan and its `--yes-plan` digest, and exit 0 **only when every origin involved is already trusted and no toolchain change is planned**. If any origin is untrusted, the preview fetches nothing from it, prints the untrusted origins and exits 3. A planned toolchain change also exits 3, and nothing is installed. Precisely: `--preview` writes nothing outside a per-run staging directory in the manager's directory under the user's cache directory (from the `Platform` port, outside `state/`), which holds fetched objects and temporary trees. It writes no ledger, receipt, journal entry, snapshot, lock edit or live configuration. The staging directory is removed at exit. A preview killed by SIGKILL leaves it behind, and the next run of any manager command removes stale staging directories. Preview of `sync` and `update` uses the network for the fetch into staging from trusted origins. `--preview` together with `--yes-plan` is refused with exit 2. The digest is computed fresh on every run. The manager never stores it and never reads one from a cache. A reviewer must read the preview before passing the digest back. The engine cannot verify that a person read it. That limit is recorded.
5. `--yes-plan <digest>` recomputes the plan digest and compares. A mismatch exits 3 as stale. Approval kind in the receipt is `yes_plan`. It does not waive: per-operation prerequisite checks, secret credential availability (unavailable gives exit 3), privilege availability, disruptive actions (a separate `--allow-disruptive <operation-id>` bound to the same digest), new-origin trust (interactive only), or a toolchain install (interactive only).
6. **Operation bounds.** `--timeout` also bounds each provider operation and external process; the default is 600 s per operation. An adapter may lower the bound. It may raise it for a named operation only (for example a native completion wait) up to the run limit, and the raised bound appears in the plan and the digest. The run limit is 4 h. When the run limit expires, the manager stops starting operations, treats the current operation as timed out, marks unstarted operations `not_started`, and exits 1 with `E_RUN_LIMIT`. On any timeout the manager sends SIGTERM to the process group, waits a 10 s grace period, then sends SIGKILL to the group. The operation is `failed`, or `pending` if the provider already returned a completion token.
7. **Two-signal escalation.** First SIGINT or SIGTERM: stop starting new operations, let the current operation finish for up to 30 s, then send SIGTERM to its process group, wait the same 10 s grace period as for a timeout, then send SIGKILL to the group. Write the receipt and exit 130 (SIGINT) or 143 (SIGTERM). Second signal, of either kind: send SIGKILL to the process group immediately, leave the `intent` journal entry as the interruption record, write what receipt state can be written, and exit with the **first** signal's code (130 or 143).
8. Disruptive set (decision 8). Disruptive: restart or unload of a user launch agent or Homebrew service (for example the window manager, status bar and border helpers), any configuration change that reloads a window manager, and deletion of an owned file. Not disruptive: writing a configuration file that is not yet loaded. Each disruptive operation needs `--allow-disruptive` or interactive confirmation.
9. No scheduler, daemon, background process or login item is part of this contract.
10. **Lock (F4, N3, N7).** One manager lock: an advisory `flock` on `~/.local/state/dotfiles-manager.lock`, a file in the parent directory, holding the pid and start time. It is outside `state/`, so state upgrade renames do not move it. Mutating commands and `verify --mark` take it exclusively. A second one gets exit 3 `E_LOCKED`. Read-only commands take no lock. They open the `state` directory by descriptor once at start and read relative to that descriptor, so they see the old or the new directory consistently during an upgrade rename. If no `state/` directory exists at that moment, they retry once after 1 s and then refuse: exit 3, or for `doctor` a finding and exit 1.

## 11. Typed adapter boundary and shared contracts (C01, C09, C10)

Adapters are Rust types compiled into the binary and selected by the versioned identifier in the manifest (`fs-link/v1`). An unknown identifier is `E_ADAPTER_UNKNOWN` at resolve time.

### Adapter trait (shape, not final code)

```rust
trait Adapter {
    fn id(&self) -> AdapterId;                              // "fs-link/v1"
    fn validate(&self, value: &Value) -> Result<Typed, SchemaError>;
    fn claims(&self, t: &Typed) -> Claims;                  // outputs, inputs, owners, sensitivity
    fn probes(&self, t: &Typed) -> Vec<ProbeRequest>;       // from the allowlist only
    fn plan(&self, t: &Typed, obs: &Observation) -> Result<Vec<Operation>, PlanError>;
    fn apply(&self, op: &Operation, cx: &mut ApplyCx) -> Outcome;
    fn postcondition(&self, op: &Operation) -> Check;
    fn already_done(&self, op: &Operation, obs: &Observation) -> bool;
}
```

`validate` and `claims` are pure. Only `apply` touches the world, through `ApplyCx` ports. `Outcome` is `Done | Failed(code) | Blocked(code) | Pending(token) | Unknown(reason)`.

### Shared ports (the contracts workflow tests use)

| Port | Methods (summary) | Result types |
| --- | --- | --- |
| `Installer` (installation and discovery) | `discover(tool) -> Vec<Candidate{owner, versions, paths, config_hints}>`; `owner_of(component)`; `install(component, version)`; `uninstall(component)` | `Outcome`; discovery never mutates |
| `Inspector` | `inspect(ProbeRequest) -> Observation{value or UNKNOWN(reason), observed_at, freshness}` | Allowlist enforced by the runner |
| `Generator` and `Reloader` | `generate(inputs) -> Output`; `reload(scope) -> Reloaded | Requested | Unsupported | Failed` | A generated file is not a reload |
| `UpdateProvider` | `candidates(policy)`; `request(candidate) -> Token`; `completion(token) -> Queued | Completed | Failed | Unknown` | Queued is `pending`. Only `Completed` is `updated`. |
| `Fs`, `Clock`, `Runner`, `Vault` | Filesystem (descriptor-relative), time, allowlisted process spawn, secret access | Replaceable per test |

### Fake implementations

1. In-process fakes implement the same traits against an isolated temporary HOME. They model: probe timeout, malformed output, queued-then-completed updates, queued-then-never-completed updates, partial install, empty secret, vault unavailable, a concurrent foreign edit between plan and apply, and a symlink swapped into a parent between check and write.
2. Process-level fakes are small executables on an isolated `PATH`. They record argv, cwd and environment so tests can assert allowlist compliance and the absence of network or write calls.
3. A conformance suite runs against every implementation of a port: the fake and, at integration, the real adapter. A fake that fails the suite is a bug in the fake.

### Verification layering (C10)

1. Workflow leaves (update, sync, try/adopt/purge, doctor) test orchestration only: ordering, blocking, approval binding, receipts, accounting, exit codes. They use the fakes and the conformance suite.
2. A workflow leaf does not implement a real provider adapter and does not invent a port. A needed port change is a change to this document.
3. Real provider adapters are built and conformance-tested in their own leaves. They are joined to the workflows at integration, in an isolated HOME.
4. Native-device behavior on the personal M5 and work M3 is the later evidence gate (section 13).

## 12. Distribution and bootstrap (C01, C06)

1. Layout: `~/.local/share/dotfiles-manager/versions/<semver>-<short-commit>/dotfiles-manager`. The active version is the symlink `~/.local/share/dotfiles-manager/current`. The command on PATH is a symlink to `current/dotfiles-manager`. Locations come from the `Platform` port.
2. Build: `cargo build --release --locked --offline` from the staged tree after `cargo fetch --locked`, under the build containment rules and limits in section 8. Whether the linker's ad-hoc signature is accepted under each device's security policy is **unverified** (device policy unknown). It is a device-gate item.
3. **Bootstrap provenance (S4).**
   1. The owner supplies, out of band, the bootstrap script's pinned commit and its SHA-256. The script is obtained by cloning that commit over `https` from the canonical origin, using the same scrubbed Git environment as section 8 (not inherited Git configuration), and verifying the checksum before it runs. No `curl | sh`.
   2. The script requires Git and Command Line Tools. If Command Line Tools are missing it runs `xcode-select --install`, then **polls** `xcode-select -p` and a compiler probe until they succeed or a 30-minute limit expires (the install command returns early). On expiry it stops with a clear message.
   3. It requires the exact pinned Rust toolchain version, not a channel. It does not install a toolchain silently. If absent, it prints the exact pinned version and the toolchain source and stops.
   4. **Order.** The first bootstrap cannot call the manager, because it builds the manager. So the first bootstrap implements the section 8 build controls **inside the pinned, checksummed script itself**: a checker that is part of the same pinned script commit. The checks are: the installed toolchain tree digest against the lock (section 8, item 4a); rejection of a staged `.cargo/config.toml`, legacy `.cargo/config`, `rust-toolchain` and mismatched `rust-toolchain.toml`; the path-dependency rule over every `Cargo.toml` including `[patch.*]` and `[replace]`; the `Cargo.lock` registry-only check; the ancestor check on the build directory and every ancestor (`E_BUILD_CONFIG`); and the pinned `cargo` execution model with the empty environment. The `self-check --build-controls <staged tree>` shim (section 8, item 3a) is for **later** builds only, when a manager already exists. Only after all checks pass does the script run `cargo fetch --locked` with crates.io as the only registry, build offline, run `self-check --fixture-dir`, and install into the layout. The containment limits in section 8 apply too. Gate tests (a) to (e) of section 8 run on the script path as well as the manager path.
   5. `cargo deny` and `cargo audit` or `cargo vet` run in the gate for the commit being built. The first self-built binary is trusted because it was built locally from the verified commit. Its SHA-256 is recorded as the first lock `[manager]` entry.
4. Old version directories follow the rollback window in section 4.

## 13. Validation against fixtures and one concrete operation (C06)

**Status: prose-validated only.** Nothing in this section was executed. Tables show **expected** behavior under this contract. They are not results.

### Expected mapping of existing cases

`examples/cases.toml` has four composition cases and 12 negative cases. A later resolver leaf must execute them. `just check` does not.

| Cases | Contract stage | Expected result |
| --- | --- | --- |
| `personal-composition`, `external-composition`, `homelab-composition`, `server-composition` | Resolve (section 6) | Module list and effective values match. The `origins` entry for `desktop.aerospace` expects provenance with the supplying module and source. |
| `unknown-module`, `duplicate-module-selection`, `module-dependency-cycle`, `unsupported-module-platform`, `external-core-replacement` | Resolve: selection, dependency, platform | Exit 3 with the case's code. No state touched. |
| `protected-core-source`, `competing-default-overrides`, `contribution-namespace`, `output-owner-collision` | Resolve: authority and claims | Exit 3. Section 6 items 2 and 3 produce the codes. |
| `missing-external-source`, `source-containment`, `home-target-containment` | Resolve: source and target containment | Exit 3. Section 6 item 5 and section 8 apply. |

The first error follows the validation order in DESIGN.md section 9. This contract adds `E_SCHEMA_UNSUPPORTED`, `E_STATE_NEWER`, `E_SOURCE_UNTRUSTED`, `E_ADAPTER_UNKNOWN`, `E_HOME_MISMATCH`, `E_LIMIT_EXCEEDED`, `E_LOCKED`, `E_CONFLICT`, `E_RUN_LIMIT`, `E_TOOLCHAIN_MISMATCH` and `E_BUILD_CONFIG`. None has a fixture yet.

Gap: the fixtures contain no case for schema refusal, state-version refusal, an oversized document, a new-origin gate or an interrupted operation. Fixtures for these codes and scenarios are deferred to the implementation task that owns each (resolver, state upgrade, trust, limits, interruption). This contract does not add unexecuted fixtures.

### Expected walkthrough: `sync` on `personal-dev-01`

Scenario: `sync` pulls a core revision that changes several resources. The profile composes `core`, `agents`, `desktop`, `personal-dev`. Names are the synthetic fixture names.

1. **Resolve.** Every `schema` is 1, accepted. Sources are fetched into staging with the isolated Git environment. The manager recomputes each tree digest and checks reachability from the ref. The lock change, old and new commit, digests and diff stat are part of the plan. The resolver closes `requires`, applies overrides, and produces the effective model with provenance.
2. **Inspect.** Allowlisted probes only. `lstat` and `readlink` on `~/.config/git/config` and `~/.ssh/config`. A bounded read of `~/.claude/settings.json`. An installed-version probe for `atuin` (a candidate probe, `UNKNOWN` until its fixture test exists). Application bundle metadata for `firstmate`. Existence and mode of the secret destination. No `--version` run, no network.
3. **Plan** (participants and expected status):
   - File `core.ssh` (`fs-link/v1`): link absent, so `create-link`; prerequisite "leaf absent, parents are real directories"; postcondition "`readlink` equals source". Expected `updated`.
   - Co-owned `agents.claude-settings` (`claude-settings/v1`): `merge-fields` for managed fields and handlers only. The prerequisite fingerprint is a salted hash of the managed fields only. The adapter re-reads the live file at apply time and preserves foreign hooks. Class `sensitive`, so no snapshot content. Expected `updated`.
   - Package `personal-dev.atuin` (`homebrew/v1`): the locked pin differs, so `sync` applies it and does not float to a newer release. If the observed owner is not the declared owner, expected `blocked`.
   - Secret `personal-dev.example-secret` (`op-template/v1`): secrets are not in `sync` scope by default. Expected `not_in_scope`. The plan shows destination and mode 0600, no content, no hash.
   - Native `personal-dev.firstmate` (`firstmate-native/v1`): `sync` applies declarations only and requests no native update. Expected `current`.
   - Manager component, only if the lock changes its version: last, per section 8.
4. **Approve.** Interactive: the plan is shown and the digest recorded with `approval.kind = interactive`. Noninteractive: `--yes-plan <digest>` from a prior `--preview`, or exit 3 with the unsatisfied gate. A new origin is never approvable this way.
5. **Apply.** Journal `intent` per operation. Before writing `settings.json`, the adapter re-reads the live file through the descriptor-relative `Fs` port. A fingerprint mismatch makes the operation `blocked` (stale) and blocks dependents. Otherwise it writes an `O_EXCL` temporary file in the same directory, validates it, and `renameat`s it.
6. **Verify.** Each postcondition is tested. The reload of any running consumer is a separate result (`Requested` or `Unsupported`), never silently `Reloaded`.
7. **Result.** Expected counts: `updated` 3, `current` 1, `not_in_scope` 1, others 0 (sum 5 participants). Ledger and receipt written. Exit 0. A repeat run expects an empty plan and exit 0.
8. **Failure variant.** If the package install fails, mid-run: the operation is `failed`, dependents `blocked`, unstarted operations `not_started`. Completed file operations stay `updated` in the receipt and ledger. Exit 1. The next run reads the journal, runs `already_done` for each operation, and resumes. No cross-provider rollback.
9. **Pending variant.** Under `update`, the `firstmate` request returns a token and `completion(token)` is `Queued`. The status is `pending`, the journal phase is `pending`, and with no failure the exit code is 5.
10. **Secret variant.** With the secret scope explicitly selected, the manager authenticates only if the credential is already available. A failed or empty render leaves the previous file untouched. A noninteractive run without the credential exits 3 before mutation.

### Later evidence gate: device and bootstrap compatibility

Not assumed. The following need evidence on the real personal M5 and work M3 in a later leaf, and are unknown until then. Gate items are named for reference:

- **DG-1, Cargo configuration discovery and build environment.** Owner role: the repository owner (the person who accepts this contract). First device: the personal M5. The gate confirms with the pinned Cargo that configuration is discovered only from the build directory, its ancestors and `CARGO_HOME`, as section 8 assumes, that the empty build environment is enough for the linker and Command Line Tools to work with no inherited variable, and that gate tests (a) to (e) give the stated results on both the manager path and the bootstrap script path. These claims are **unverified**. **Consequences:** until DG-1 passes, `update` refuses to build the manager (`E_BUILD_CONFIG`) and bootstrap stops. If DG-1 fails, the section 8 build rules are revised before this contract is accepted. **Known device limit:** a user `~/.cargo/config.toml` that lies on the build directory's ancestor chain makes self-update fail closed with `E_BUILD_CONFIG`, and the manager does not work around it.

Other unknowns: Command Line Tools and the exact pinned Rust toolchain on each device, linker behavior and ad-hoc signing under each machine's security policy, Homebrew and mise probe behavior without side effects, noninteractive vault availability, filesystem alias behavior, availability of the Git protocols, credential helper and `ssh` host-key policy for the declared origins, and whether the work M3's policy permits building and running a locally built binary.

## 14. Decisions and open questions

### Decided (owner, 2026-10-09)

1. **Build requirement.** Git, Command Line Tools and a pinned Rust toolchain are accepted at bootstrap only. Bootstrap polls for Command Line Tools completion before building. Rationale: the static binary removes the interpreter-retirement risk (section 2). Python 3.14 standard library through uv stays documented as the fallback.
2. **Release artifacts.** Build from source for first delivery. No signing or release process. Rationale: avoids a release and signing decision before the device gate.
3. **Crates.** `serde`, `toml`, `serde_json` and `sha2` are accepted as the four direct crates, with exact version pins, locked transitives and a `cargo-deny` gate. Rationale: the standard library has no TOML parser or SHA-256.
4. **Commit signatures.** Not required now. Trust is origin plus the recomputed tree digest. Signed commits can come later. Rationale: no signing infrastructure exists, and the digest binds content.
5. **Exit codes.** 3, 4, 5 and 130 accepted, with 143 for SIGTERM.
6. **Names.** Keep `dotfiles-manager` and `~/.local/state/dotfiles-manager/`.
7. **Prompt timeout.** 300 s accepted.
8. **Disruptive set.** The set in section 10 is proposed and accepted as the working definition.
9. **Firstmate scope.** `firstmate` applies to the personal M5 only. The work M3 profile does not include it. M3 is deferred and not a first-delivery requirement.
10. **Final-round decisions.** `doctor` stays read-only (marker moved to `verify --mark`). Build-script risk is accepted and documented with no sandbox. Missing error-code fixtures are deferred to the owning implementation tasks. `blocked` stays exit 1, and a run with no failure but a pending operation exits 5.

### Open risks (owner review)

| Risk | Status | Concrete fix | Closed by |
| --- | --- | --- | --- |
| Cargo configuration discovery works as section 8 assumes | Unverified | Run gate tests (a) to (d) with the pinned Cargo on both the manager and script paths | DG-1 |
| The empty build environment still lets the linker and Command Line Tools work, and a planted rustup proxy is never invoked | Unverified | Run gate test (e) and a full build on the personal M5 | DG-1 |
| Ad-hoc code signing of a locally built binary is accepted by the device's security policy | Unverified | Build, run and `self-check` on the personal M5 | DG-1 (first device only; the work M3 is deferred with firstmate scope, but its policy remains unknown) |
| Build scripts and procedural macros run as the user with network and HOME access | Accepted, not closable by a gate | Rely on origin trust, tree digest, lock diff review, and the CI audit gates. A sandbox is a possible later change | None; recorded risk |
| `--yes-plan` assumes a person reviewed the preview | Cannot be verified by the engine | None in this contract | None; recorded limit |
| Probe allowlist entries are candidates only | Unverified | Fixture-backed tests at implementation, one per probe | Implementation task for each adapter, not DG-1 |

### Open questions for the owner

1. **Employer identity.** The public documents use "work M3" and a synthetic "work" module, and name no employer. Confirm that no document, fixture, lock origin or receipt field in this public repository may identify the employer or work-machine details, and that private module repositories carry them.
2. **Device gate.** Which device runs the section 13 gate first, and who runs it? Until it runs, the Rust choice and the bootstrap sequence are unverified on both devices.

## 15. Not in this contract

No marketplace, expression language, daemon or scheduler, Linux or Intel provider, dynamic adapter loading, or cross-provider transaction. Renaming existing tool or device names is out of scope here.
