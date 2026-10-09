# Minimum implementation contract

Status: **Proposed; owner review pending.** Task: DFM-10. This document selects the implementation language and fixes the minimum formats and boundaries. It implements nothing. [DESIGN.md](../DESIGN.md) remains the requirements source. This document takes precedence over DESIGN.md section 11 only after the owner accepts it.

Criteria covered: C01 to C06 and C08 to C10. C07 (independent review) and C11 (delivery evidence) remain open.

## 1. Decision summary

1. Language: Rust (stable, edition 2024), one binary, no runtime interpreter.
2. Dependencies: the Rust standard library plus four vendored crates (`serde`, `toml`, `serde_json`, `sha2`). Hand-written argument parsing. No async runtime, no plugin loader, no expression language.
3. Adapters: compiled into the binary, behind typed Rust traits. There is no marketplace and no dynamic loading.
4. Formats: TOML for authored documents and source locks. JSON for machine-written state and receipts. Every document carries an integer `schema`.
5. Distribution: the binary installs into a manager-owned versioned directory. A symlink selects the active version. The coordinator updates itself last.

## 2. Prior-art check and rationale record

Hippo query (`mcp__hippo__ask` and `search_hybrid`, 2026-10-09): no prior record selects an implementation language for this manager. Related history exists: the owner evaluated chezmoi, Nix, Stow, YADM and Dotbot for the current dotfiles (chezmoi needed repeated re-adds for live edits, Nix was heavy for frequent small edits, Stow had no generation), and a chezmoi to nix-darwin port was already tried on a branch. Those results motivate a replacement manager. They say nothing about its language. The `ask` call itself was degraded (inference backend unreachable). The conclusion rests on `search_hybrid` results only.

### Language trade-offs

Verified on the authoring machine (Apple Silicon): `/usr/bin/python3` is 3.9.6 and has no `tomllib`. Python 3.14.7, uv 0.12.17, mise, Rust 1.98.1 and Apple Swift 6.4 are installed. Python, uv and Rust arrive through mise or Nix shims, so the manager itself can update or retire them.

| Option | For | Against |
| --- | --- | --- |
| Python 3.14, standard library | Zero third-party code. `tomllib`, `json`, `hashlib`, `os.replace` exist. The repo already uses it for `scripts/check_examples.py`. Fastest to write. | Needs a managed interpreter at run time. `update` can retire that interpreter mid-operation. A fresh machine has no suitable Python (system Python lacks `tomllib`), so bootstrap needs uv first. Weak static typing at the adapter boundary. |
| Swift | OS-owned toolchain via Command Line Tools. Native Foundation, CryptoKit. | No TOML in the standard library or Foundation. Foundation differs on Linux. Smaller ecosystem for TOML and JSON Pointer. Ties core code to Apple tooling (conflicts with C04 intent). |
| Go | Static binary. Rich standard library. | No TOML in the standard library. Weaker enum modelling for typed operations and outcomes. Toolchain arrives through mise. |
| Rust | Static binary: nothing the manager can retire is needed at run time. Enums and traits express operations, outcomes and adapter ports with exhaustive matching. Fixed supply chain through `Cargo.lock` and vendoring. | No TOML in the standard library, so four vetted crates. Slower to write. Build needs a Rust toolchain and the Apple linker. |

Decision: Rust. The deciding facts are bootstrap (C01, C05) and self-update. A compiled binary does not depend on an interpreter that `update` may replace, and the type system enforces the adapter contract. The cost is four crates and a build step. Python is the fallback if the owner rejects the build requirement (open question 1).

## 3. Toolchain and repository layout (C01)

- `rust-toolchain.toml` pins the channel. `Cargo.lock` is committed. `cargo vendor` output is not committed. CI and the local gate use `--locked --offline` after a fetch step.
- Workspace layout, with this dependency direction only (arrows point at what is depended on):

```
cli  ->  engine  ->  model
cli  ->  platform_macos  ->  engine ports
engine defines ports (traits); platform_macos and test fakes implement them
```

- `model`: documents, schema validation, resolver, provenance. Pure functions. No process, network or filesystem calls except through the `Fs` port.
- `engine`: planner, applier, receipts, state, journal. Defines the ports in section 11.
- `platform_macos`: Homebrew, mise, launchd, application, `op` and filesystem-alias behavior. The only crate that names macOS paths, tools or APIs.
- `cli`: parsing, rendering, exit mapping.
- Existing checks stay. `scripts/check_all.sh` gains `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test --locked` only when the workspace exists. The shared hook and CI script rule in AGENTS.md is unchanged.

## 4. Versioned schema and compatibility (C01, C03)

Every authored and machine-written document has an integer top-level `schema`. Document kinds: `bundle`, `module`, `profile`, `lock`, `binding`, `state`, `receipt`, `journal-entry`.

Rules:

1. The binary has a compiled list of supported schema versions per document kind. Initially `[1]` for each.
2. An unlisted version, higher or lower, is refused before any other field is read. The error is `E_SCHEMA_UNSUPPORTED`. It names the file, found version and supported list. Exit code 3.
3. Schema 1 accepts known keys only (DESIGN.md section 3). Any added key is a new schema version. The manager never ignores an unknown key.
4. Each kind has a compatibility statement in its owning release notes: readable versions and written version. The binary writes only its newest version for each kind.
5. Authored documents are never rewritten by an automatic upgrade. A schema migration of authored files is an explicit, reviewed edit.

### State upgrade and recovery

State lives under `~/.local/state/dotfiles-manager/` (mode 0700; files 0600).

1. Upgrade is forward only and copy based. The manager reads the old state, writes the new format into `state.next/`, validates it, writes `state.next/COMPLETE`, then renames `state` to `state.prev-<old schema>` and `state.next` to `state` (two renames; the order is fixed).
2. Recovery by directory shape: `state.next/` without `COMPLETE` is discarded and the upgrade restarts from untouched `state/`. `state.next/COMPLETE` with no `state/` completes the rename. `state/` and `state.prev-*` both present is normal until the next successful run, which removes `state.prev-*`.
3. State newer than the binary supports is refused (`E_STATE_NEWER`), exit 3. There is no downgrade. The recovery is `state.prev-*` plus an older binary version, which stay on disk by the distribution rule in section 12.
4. Receipts are immutable. An old receipt with an unsupported schema is treated as unreadable. Its outputs have unknown ownership, so no deletion is authorized from it. This matches the existing rule that state loss cannot make unknown files owned.
5. Unreadable or missing state never triggers forced apply. The only exits are `doctor` (read-only), inspect-and-adopt, or restoration from a receipt snapshot.

## 5. Document formats (C01)

Authored formats are the existing fixtures: `bundle.toml`, `module.toml`, `profile`. Nothing in this section changes them.

### Source lock

`dotfiles-manager.lock` is TOML. It lives in the source that owns the pin (DESIGN.md section 7: never private module details in the public core). The manager writes it only during `update` or `sync` after approval, as a reviewable local edit with no automatic commit.

```toml
schema = 1

[[sources]]
id = "example-external"           # bundle or module source id
kind = "git"                      # "git" or "path"
origin = "https://example.invalid/org/repo.git"  # canonical form, see section 8
ref = "refs/heads/main"           # tracking ref, informational for exact pins
commit = "<40 hex>"               # resolved revision
tree = "sha256:<64 hex>"          # content identity of the checked-out tree (section below)

[[adapters]]
id = "homebrew/v1"
required_by = ["personal-dev.atuin"]
```

Content identity: SHA-256 over a sorted list of `path NUL mode NUL file-sha256` entries for tracked files. For `kind = "path"` (local editable source) the lock stores no commit. Any change to such a source invalidates an existing plan, as DESIGN.md requires. Adapter entries record the adapter identifier and version the lock was produced with. The lock holds no secret values and no hash of secret material.

### Receipt

JSON, one file per operation run: `receipts/<utc-timestamp>-<op-id>.json`, canonical form (UTF-8, sorted keys, no insignificant whitespace). Fields:

```
schema, op_id, command, started_at, finished_at, binary{version, commit},
profile{id, binding_digest}, plan_digest, approval{kind, plan_digest, at},
sources[{id, commit, tree}],
operations[{id, resource, adapter, kind, status, started_at, finished_at,
            before{fingerprint}, after{fingerprint}, owned_outputs[], error?}],
result{outcome, counts{updated, current, held, pending, failed, blocked, not_started}}
```

- `status` is one of `done`, `failed`, `blocked`, `not_started`, `not_in_scope`, `pending`, `skipped_hold`.
- `fingerprint` is a SHA-256 of file content or a stable native identity (for example a package name and version). For secret outputs, the receipt stores the destination, mode and the label `secret`. It stores no content and no hash (DESIGN.md section 8).
- `owned_outputs` is the exact set the manager may later remove. It is the only source of delete authority.

### Journal and interruption records

`journal.jsonl` is append-only, one JSON object per line, `fsync` after each append. Before each mutating operation the engine appends `{"op_id","operation","phase":"intent", "preconditions_digest"}`. After the operation and its postcondition check it appends `phase:"done"` or `phase:"failed"`. An `intent` without a terminal entry is an interruption record. The next invocation reports it, runs the operation's idempotence probe (section 9), and either marks it `done`, marks it `failed`, or reports `unknown` and refuses dependent work. Interruption never authorizes deletion.

### Binding

`~/.config/dotfiles-manager/machine.toml` (DESIGN.md section 3): `schema`, canonical profile path, machine identity, expected platform and architecture, and the trusted origin set (section 8). One binding per machine.

## 6. Authority model in code (C02)

DESIGN.md sections 2, 3 and 5 define the rules. This section records how the implementation holds them.

1. **One bound profile.** `BoundProfile` is constructed only from the binding file. The CLI has no `--profile` flag on mutating commands. Tests use `DFM_BINDING` to point at an isolated binding.
2. **Protected core.** The resolver marks every core path not listed in `policy.defaults` or `policy.extensions` as protected. A change request to a protected path returns `E_OVERRIDE_FORBIDDEN`. Core cannot name a non-core module in `requires`.
3. **Explicit defaults and contributions.** Only the two policy lists admit change. An override or contribution is a typed `Change { resource, path, value, reason, origin }`. There is no generic merge function in `model`.
4. **Provenance.** Every effective value carries `Provenance { owner, original, change?, changer, reason, source_file, content_id }`. `explain` prints it. Secret-bearing values hold references only.
5. **Private-to-public direction.** A bundle can `extends` exactly one ancestor and cannot be referenced by it. The resolver rejects a public source whose closure reaches an external source. Module paths resolve against the declaring manifest and must stay inside its trusted root.
6. **Order independence.** The resolver sorts only for display (core first, then lexical). Conflict detection runs on sets, not sequences.

## 7. OS separation (C04)

1. `model` and `engine` contain no path literal for a prefix (`/opt/homebrew`, `/usr/local`), no machine model name and no tool name. A grep-style check in the gate enforces this for those crates (a test, not a convention).
2. HOME is derived once, at startup, by `platform_macos::home()`: the password-database home of the effective user, which must equal `$HOME` unless `DFM_HOME` is set. `DFM_HOME` is for isolated tests only and the binary prints it in every report header when set. A mismatch without `DFM_HOME` is `E_HOME_MISMATCH` and fails before mutation.
3. Provider locations are observations, not constants. Each provider adapter declares an ordered candidate search (for example, locations conventional for the platform and architecture) and verifies the result (file exists, executable, owned by an expected user). The verified location is recorded in the report and receipt. `doctor` shows when a candidate fails verification.
4. The OS-bound surface is the `platform_macos` crate: filesystem case and alias behavior, launchd, application bundles, Homebrew, mise, `op`. The `Platform` port exposes `home()`, `case_fold(path)`, `locate(provider)` and `runner()`. Composition, planning and state use only the port.
5. Optional GUI or service resources (desktop, launchd agents) exist only in modules a profile selects. The engine has no GUI assumption. Without a selected desktop module, no GUI probe runs.
6. Not built: Linux providers, Intel-specific paths, portability layers. The only commitment is that nothing in `model` or `engine` would need to change to add them.

## 8. Trust, probes, privilege, snapshots (C05)

### Trusted source identities

A source identity is `(canonical origin, resolved commit, tree digest)`.

1. Canonical origin: lowercase host, no credentials, no trailing `.git`, `https` or `ssh` scheme only (`path` sources are local and unlocked).
2. The binding holds the trusted origin set. A new origin needs explicit approval during an interactive `sync` or `update`. A fetched manifest never grants trust, and fetching never runs repository code.
3. A fetched commit is accepted only when its tree digest matches the lock, or the approved plan includes the lock change. Commit signature verification is not required in this contract (open question 4).
4. Unknown origin, redirect to a different origin, or a changed tree under an unchanged pin: `E_SOURCE_UNTRUSTED`, exit 3, no mutation.

### Read-only probe allowlist

A probe is data owned by an adapter: `{ argv (exact), env (scrubbed, plus fixed vars), cwd, timeout, max_output, expects }`. The runner executes only entries whose argv equals a declared entry after placeholder substitution of validated identifiers. Rules:

1. Allowed without any entry: `lstat`, `readlink`, directory listing and bounded reads of declared paths.
2. Never allowed: running a discovered or shimmed binary for `--version`, any network call, vault call, install, update or repair.
3. Environment is rebuilt from an allowlist (for example `PATH`, `HOME`, `LANG`) plus adapter variables that disable automatic update and analytics. Stderr is captured, truncated and redacted before display.
4. Each entry is added only with a fixture-backed test that shows no write and no network. Candidate entries to verify in the implementation phase (not verified here): a Homebrew installed-version listing, a mise installed-tool listing, a launchd service listing, reading an application bundle's metadata file. Unverified candidates stay disabled, and the probe result is `UNKNOWN`.
5. A timeout, a non-allowlisted request or an unsupported platform fact yields `UNKNOWN` with a reason. Unknown support fails closed: no apply operation proceeds on an unknown prerequisite.

### Prerequisites and postconditions

Every typed operation declares both, as data checked by the engine:

- Prerequisites: observation fingerprints that must still hold at apply time, source identities, privilege class, credential availability, quiescence requirement.
- Postcondition: a probe or file check that proves the effect (file content matches, package version observed, consumer reloaded). A file that is correct, an executable that is installed and a consumer that has reloaded are three separate postconditions.
- A failed prerequisite yields `blocked` with a code. A failed postcondition yields `failed`. Neither is ever reported as success.

### Privilege boundaries

- Operation privilege classes: `user` (default), `privileged` (explicit scope, separate adapter), `native-ui` (requires the application's own prompt).
- HOME adapters never escalate. A privileged operation appears in the plan with its exact command class and scope. Execution uses a non-interactive privilege check first. If the privilege is not already available and no TTY exists, the plan fails before any mutation (section 10).
- The manager never stores a privilege credential.

### Snapshot privacy

- Snapshots exist only for adapters that declare restore support (ordinary files). Location: `state/snapshots/<op-id>/`, directories 0700, files 0600.
- Secret outputs are never snapshotted. Their last-good file stays at the destination and the new content is written to a same-directory 0600 temporary file, then renamed on success.
- Receipts reference snapshots by path and fingerprint, never by content. Snapshots are removed with their receipt retention policy; deleting a snapshot is never an implicit side effect of purge.

### Coordinator self-update last

1. The manager package is a managed component like any other, with one extra ordering rule: it is always the final operation of the plan, with edges from every other operation.
2. Before it runs, the receipt for all prior operations is written and synced.
3. The new version builds or installs into a new versioned directory, runs `dotfiles-manager self-check` from that directory (loads the binding, state and lock, no mutation), and only then flips the active symlink in one atomic rename.
4. The running process keeps its old inode and exits after writing its result. The previous version directory stays until the next successful run of the new version. Rollback is a symlink flip back.
5. A failed self-check leaves the previous version active and reports `failed` for that operation only.

## 9. Operation lifecycle and idempotence

The stages are DESIGN.md section 6 (resolve, inspect, plan, apply, verify). Additions:

1. A plan has a digest over: resolved model, source identities, observation fingerprints and the ordered operation list. Approval binds to this digest (section 10).
2. Each operation implements `already_done(observation) -> bool`. After an interruption the engine uses it to avoid repeating a completed side effect.
3. On the first failure, dependents become `blocked` and independent operations that have not started become `not_started`. The default is to stop. A later `--continue` is not in the minimum contract.
4. The receipt is written even on failure and on interruption by signal. SIGINT and SIGTERM stop after the current operation, write the receipt, and exit 130.

## 10. CLI and exit semantics (C01, C08)

### Exit codes

| Code | Name | Meaning |
| --- | --- | --- |
| 0 | `ok` | Completed. No work needed counts as completed. |
| 1 | `failed` | Operation failed, diagnosed errors, incomplete required checks, or partial application. Compatible with the `doctor` contract. |
| 2 | `usage` | Invalid invocation, or inability to produce the requested report. Compatible with the `doctor` contract. |
| 3 | `blocked` | A precondition failed before any mutation: stale plan, missing credential, no TTY for a required prompt, privilege unavailable, untrusted source, unsupported schema or state version, unknown support. |
| 4 | `declined` | Approval was refused or not given. No mutation. |
| 5 | `pending` | All operations that could finish did, and at least one is pending (for example a queued native update). Not success. |
| 130 | `interrupted` | Stopped by signal after the receipt was written. |

Every non-zero result prints one stable code (`E_...` or a `doctor` finding code) and the first failing location. `--json` output repeats the code and exit name, and uses the same semantics as the table. Resolver error codes are the `expected_error` values already in `examples/cases.toml`.

### Command surface

DESIGN.md section 7 defines the commands. Global flags: `--json`, `--no-input`, `--yes-plan <digest>`, `--timeout <duration>`, `--home <dir>` (tests only; same as `DFM_HOME`). No flag selects a profile on a mutating command.

### Bounded noninteractive behavior

1. Interactive means both stdin and stderr are TTYs and `--no-input` is absent. Otherwise the run is noninteractive.
2. Before any mutation, the engine computes every prompt the plan could need (approval, new-origin trust, credential, privilege, disruptive action). In a noninteractive run, any such prompt that is not already satisfied ends the run with exit 3 and the list of unsatisfied gates. It does not wait.
3. Prompts in an interactive run have a bounded wait (default 300 s, `--timeout`). Expiry ends the run with exit 4. There is no indefinite prompt.
4. Approval for noninteractive use is `--yes-plan <digest>`. The digest binds the exact reviewed plan (section 9). A changed source, observation, privilege or operation list changes the digest, and the run exits 3 as stale.
5. `--yes-plan` does not waive: stale-state checks, secret authentication (a secret scope needs a credential that is already available non-interactively, else exit 3), privilege availability, or a disruptive action (needs a separate `--allow-disruptive <operation-id>` bound to the same digest).
6. No scheduler, daemon, background process or login item is part of this contract. Each invocation is one process that exits.
7. Locking: one manager lock per state directory (`state/lock`, advisory, with holder pid and start time). A second mutating invocation exits 3 with `E_LOCKED`. Read-only commands do not take the mutating lock.

## 11. Typed adapter boundary and shared contracts (C01, C09, C10)

Adapters are Rust types compiled into the binary and selected by the versioned identifier in the manifest (`fs-link/v1`). An unknown identifier is `E_ADAPTER_UNKNOWN` at resolve time. There is no marketplace, no dynamic loading and no manifest-supplied code.

### Adapter trait (shape, not final code)

```rust
trait Adapter {
    fn id(&self) -> AdapterId;                              // "fs-link/v1"
    fn validate(&self, value: &Value) -> Result<Typed, SchemaError>;
    fn claims(&self, t: &Typed) -> Claims;                  // outputs, inputs, owners
    fn probes(&self, t: &Typed) -> Vec<ProbeRequest>;       // from the allowlist only
    fn plan(&self, t: &Typed, obs: &Observation) -> Result<Vec<Operation>, PlanError>;
    fn apply(&self, op: &Operation, cx: &mut ApplyCx) -> Outcome;
    fn postcondition(&self, op: &Operation) -> Check;
    fn already_done(&self, op: &Operation, obs: &Observation) -> bool;
}
```

`validate` and `claims` are pure. Only `apply` touches the world, and only through `ApplyCx` ports. `Outcome` is `Done | Failed(code) | Blocked(code) | Pending(token)`. No adapter returns a bare string or boolean.

### Shared ports (the contracts workflow tests use)

| Port | Methods (summary) | Result types |
| --- | --- | --- |
| `Installer` (installation and discovery) | `discover(tool) -> Vec<Candidate{owner, versions, paths, config_hints}>`; `owner_of(component)`; `install(component, version)`; `uninstall(component)` | `Outcome`; discovery never mutates |
| `Inspector` | `inspect(ProbeRequest) -> Observation{value or UNKNOWN(reason), observed_at, freshness}` | Allowlist enforced by the runner, not by each caller |
| `Generator` and `Reloader` | `generate(inputs) -> Output`; `reload(scope) -> ReloadResult` | `Reloaded`, `Requested`, `Unsupported`, `Failed`. A generated file is not a reload. |
| `UpdateProvider` | `candidates(policy) -> Vec<Candidate>`; `request(candidate) -> Token`; `completion(token) -> Queued | Completed | Failed | Unknown` | Queued is `pending`. Only `Completed` is done. |
| `Fs`, `Clock`, `Runner`, `Vault` | Filesystem, time, process spawning with allowlist, secret access | Replaceable per test |

### Fake implementations

1. In-process fakes implement the same traits and run against an isolated temporary HOME. They model failures: probe timeout, malformed output, queued-then-completed updates, queued-then-never-completed updates, partial install, empty secret, vault unavailable, concurrent foreign edit between plan and apply.
2. Process-level fakes are small executables placed on an isolated `PATH` for adapters that must spawn a tool. They record argv and environment so tests can assert allowlist compliance and the absence of network or write calls.
3. A conformance suite runs against every implementation of a port: the fake and, at integration, the real adapter. A fake that fails the suite is a bug in the fake.

### Verification layering (C10)

1. Workflow leaves (update, sync, try/adopt/purge, doctor) test orchestration only: ordering, blocking, approval binding, receipts, accounting, exit codes. They use the fakes and the conformance suite.
2. A workflow leaf does not implement a real provider adapter and does not invent a new port. A needed port change is a contract change to this document.
3. Real provider adapters are built and conformance-tested in their own leaves. They are joined to the workflows at integration, where the integration tests replace fakes with real adapters in an isolated HOME.
4. Native-device behavior on the personal M5 and work M3 is the later evidence gate (section 13).

## 12. Distribution and bootstrap (C01, C06)

1. Layout: `~/.local/share/dotfiles-manager/versions/<semver>-<short-commit>/dotfiles-manager`. The active version is a symlink `~/.local/share/dotfiles-manager/current`. The command on PATH is a symlink to `current/dotfiles-manager`. (Locations are defaults for the macOS provider; the engine reads them from the `Platform` port.)
2. Build: `cargo build --release --locked --offline` from a verified commit, with vendored sources fetched against `Cargo.lock`. The linker's ad-hoc signature is sufficient for a locally built binary. No release artifact, signing identity or notarization is part of this contract (open question 2).
3. Bootstrap on a clean machine: a short POSIX `sh` script in this repository that (a) checks for Git and a Rust toolchain at the pinned channel, (b) clones the repository at a pinned commit, (c) builds, (d) installs into the layout above, (e) runs `self-check`. It prints each missing prerequisite and stops. It installs nothing silently.
4. The bootstrap prerequisites (Command Line Tools, a Rust toolchain provider) are not assumed available on the M5 or M3. That is checked on the real devices later (section 13).
5. Old version directories are kept until the next successful run of the new version, which is the rollback window for section 4.

## 13. Validation against fixtures and one concrete operation (C06)

### Existing cases

`examples/cases.toml` has four composition cases and 12 negative cases. The contract maps them as follows. All fixtures stay unexecuted by `just check`; the resolver must execute them in a later leaf.

| Cases | Contract stage | Result |
| --- | --- | --- |
| `personal-composition`, `external-composition`, `homelab-composition`, `server-composition` | Resolve (section 6) | Module list and effective values match. The `origins` entry for `desktop.aerospace` shows provenance carrying the supplying module and source. |
| `unknown-module`, `duplicate-module-selection`, `module-dependency-cycle`, `unsupported-module-platform`, `external-core-replacement` | Resolve: selection, dependency, platform | Exit 3, code from the case. No state touched. |
| `protected-core-source`, `competing-default-overrides`, `contribution-namespace`, `output-owner-collision` | Resolve: authority and claims | Exit 3. Section 6 items 2 and 3 produce the codes. |
| `missing-external-source`, `source-containment`, `home-target-containment` | Resolve: source and target containment | Exit 3. Items 5 of section 6 and section 8 apply. |

The first error follows the validation order in DESIGN.md section 9. `E_SCHEMA_UNSUPPORTED`, `E_SOURCE_UNTRUSTED`, `E_ADAPTER_UNKNOWN` and `E_HOME_MISMATCH` are added by this contract and have no fixture yet. Each needs a case before the resolver is accepted (listed as follow-up in the PR).

Gap found: the fixtures contain no case for the schema-version refusal, a state-version refusal or an interrupted operation. This contract defines them (sections 4, 5), but nothing validates them yet.

### Walkthrough: `sync` on `personal-dev-01`

Scenario: `sync` pulls a core revision that changes five things. The profile composes `core`, `agents`, `desktop`, `personal-dev`. Paths and names are the synthetic fixture names.

1. **Resolve.** The binary loads the binding and the profile. Every `schema` is 1, accepted. Sources are fetched into staging (not the linked checkout). The staged tree digests are compared with the lock, and the changed lock is part of the plan. The resolver closes `requires`, applies overrides, and produces the effective model with provenance.
2. **Inspect.** Allowlisted probes only. `lstat` and `readlink` on `~/.config/git/config` and `~/.ssh/config`. Read of `~/.claude/settings.json`. A Homebrew installed-version listing for `atuin` (candidate probe, disabled and reported `UNKNOWN` until verified). Application bundle metadata for `firstmate`. Existence of the secret destination file and its mode. No `--version` run, no network.
3. **Plan.** Operations and digest:
   - File: `core.ssh`, `fs-link/v1`. The link target is absent. Operation `create-link` with prerequisite "leaf absent, parents are real directories". Postcondition: `readlink` equals the source path.
   - Co-owned: `agents.claude-settings`, `claude-settings/v1`. Operation `merge-fields` with the managed fields and handlers only. The observation fingerprint covers the whole live file so foreign hooks are captured. Postcondition: managed fields equal desired, foreign handlers unchanged.
   - Package: `personal-dev.atuin`, `homebrew/v1`. The locked pin is the declared version. It is a changed pin, so `sync` applies it. It does not float to a newer release. If the observed owner is not Homebrew, the operation is `blocked` (owner conflict).
   - Secret: `personal-dev.example-secret`, `op-template/v1`. `sync` does not include secret scope by default, so this operation is listed `not_in_scope`. The plan shows the destination and mode 0600, not any content.
   - Native: `personal-dev.firstmate`, `firstmate-native/v1`. `sync` applies declarations only. It does not request a native update (that is `update`).
   - The manager's own package, if the lock changes its version, is the last operation (section 8).
4. **Approve.** Interactive: the table is shown, the user approves, the digest is recorded in the receipt. Noninteractive: `--yes-plan <digest>` is required, or exit 3 with the unsatisfied gate.
5. **Apply.** Journal `intent` for each operation. Before writing `settings.json`, the adapter re-reads the live file. If its fingerprint differs from the plan, the operation is `blocked` (stale) and later dependents are blocked. Otherwise the adapter writes a same-directory temporary file, validates it, and renames it atomically. The link operation re-checks parents immediately before `symlink`.
6. **Verify.** Each postcondition is tested. The link and merged settings pass. `atuin` is verified by the allowlisted installed-version probe. The reload of any consumer is a separate result: `Requested` or `Unsupported` for a running application, never silently `Reloaded`.
7. **Result.** Receipt written with `owned_outputs`. Counts: `done` 3, `not_in_scope` 1, `pending` 0. Exit 0. A re-run produces an empty plan and exit 0.
8. **Failure variant.** If the Homebrew install fails, the self-update and any operation that depends on it are `blocked`. Independent file operations that already completed stay done and are listed in the receipt. Exit 1. The next run reads the receipt and the journal, runs `already_done` for each operation, and resumes. No rollback is attempted across providers.
9. **Secret and native variants.** With `update`, the `firstmate` request returns a token. `completion(token)` is `Queued`, so the operation is `pending` and the run exits 5. The secret operation, when explicitly in scope, authenticates only if the credential is available; a failed or empty render leaves the previous file untouched.

This walkthrough is prose validation of the contract. No code ran. Only the resolver cases are candidates for early executable checks.

### Later evidence gate: device and bootstrap compatibility

Not assumed here. The following need evidence on the real personal M5 and work M3 in a later leaf: Command Line Tools and Rust toolchain availability at the pinned channel, linker behavior and ad-hoc code signing under the machine's security policy, Homebrew and mise probe behavior without side effects, secret-vault noninteractive availability, and the home-directory and filesystem alias behavior. Until then these are unknown.

## 14. Open questions for the owner

1. **Build requirement.** Is it acceptable that bootstrap requires Git, Command Line Tools and a Rust toolchain on each target? The alternative is Python 3.14 standard library (no build, but a managed interpreter that `update` can retire).
2. **Release artifacts.** Build from source on each machine (this contract), or publish a prebuilt `aarch64-apple-darwin` binary with a pinned checksum? The second needs a release process and a signing decision.
3. **Third-party crates.** Are `serde`, `toml`, `serde_json` and `sha2` acceptable as the only runtime crates?
4. **Commit signature policy.** Require signed commits for trusted origins, or rely on origin trust plus tree digests?
5. **Exit-code table.** Are exit codes 3, 4, 5 and 130 acceptable in addition to the `doctor` codes 0, 1, 2?
6. **State directory and command names.** Keep `dotfiles-manager` and `~/.local/state/dotfiles-manager/` (still working names)?
7. **Prompt timeout.** Is a 300 s default for interactive prompts acceptable?
8. **Disruptive actions.** Which actions count as disruptive in the first delivery (for example restarting a window manager)? The list is needed before the apply leaf.

## 15. Not in this contract

No marketplace, no expression language, no daemon or scheduler, no Linux or Intel provider, no dynamic adapter loading, no cross-provider transaction.
