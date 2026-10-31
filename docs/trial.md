# Trial

The `trial` command (`try <tool>` in DESIGN.md section 7): try a tool without authoring a module. This document describes the behavior at HEAD. The package providers it reuses are in [docs/packages.md](packages.md), the state store in [docs/state.md](state.md). `adopt` (DFM-33) is in [docs/adopt.md](adopt.md). `purge` (DFM-34) is not built. The last section lists what they inherit.

| Part | Location |
| --- | --- |
| `trial` command: arguments, owner discovery, baseline, digest, install, receipt | `crates/cli/src/trial_cmd.rs` |
| Trial record fields (`TrialRecord`, `TrialPackage`, `TrialInstall`) | `crates/model/src/state.rs` |
| Record write (atomic, exclusive create, identity, claim and secret checks) and load | `crates/engine/src/store.rs` (`write_trial`, `load`) |
| Provider probes and installs (`homebrew/v1`, `mise/v1`) | `crates/cli/src/adapters/package.rs` |
| Tests with `brew` and `mise` look-alikes | `crates/cli/tests/trial.rs`, scripts in `crates/cli/tests/common/providers.rs` |

## The command

```
dfm trial <package> [--id <trial-id>] [--provider homebrew/v1|mise/v1] [--config <path>]... [--preview] [-y|--yes-plan <digest>]
```

- `<package>` is the package as the provider names it (`atuin`, `homebrew/core/atuin`, `node@20`), checked by the package adapter's name rule.
- The tool is its short name (`atuin`, `node`). It must be a valid id: lowercase ASCII letters, digits, `.`, `_`, `-`. A mise backend name such as `npm:prettier` is a usage error.
- The trial id is `trial-<tool>` unless `--id` names another.
- `trial` takes no `--root`: it reads no source tree. The machine must be bound.

Order of one run:

1. **Owner discovery.** The signal handlers go in. Then the providers are probed read-only as `packages` probes them (docs/packages.md). With `--provider`, only that provider is asked. Without it, both are asked:
   - A provider lists the package, or an executable of its name has another owner: the tool **pre-exists**. Its owners are recorded (`homebrew/v1 (18.24.1)`, or the other-owner text with its path), and nothing is installed.
   - Exactly one provider reports it missing and installable: that provider installs it, with the dependencies the provider names.
   - More than one can install it: the choice is the owner's. `E_APPROVAL_REQUIRED` (exit 3) names the candidates, with `--provider` as the fix.
   - None can: a blocker with the probe's own code (`E_PROBE_FAILED` when any provider could not answer, else `E_SOURCE_UNTRUSTED`, `E_PLATFORM_UNSUPPORTED`, `E_CONFLICT` or `E_PRIVILEGE_UNSUPPORTED`) and each provider's answer.
2. **Baseline.** The configuration candidates are the `--config` paths (absolute, or relative to HOME), or `~/.config/<tool>` when none is given. Each must pass the store's claim check (inside HOME, outside the manager's own paths) before any provider runs. Each is observed once. A regular file is hashed (`file_fingerprint`). A directory and an absent path are recorded as such. A link, a special file, an unreadable entry or a file over 16 MiB is a **baseline failure**: `E_STATE_INVALID`, exit 3, nothing written. Recording such a path as absent would let a later purge delete what was there.
3. **Preview.** The decision prints: the trial and tool, the provider and package, the action (install with its dependencies, or nothing because the tool pre-exists and who owns it), one `baseline:` line per candidate, the cleanup scope, the `unknown:` effects and `plan digest:`. `--preview` stops here, exit 0 (3 when blocked). Without `--yes-plan`, the run stops with `E_APPROVAL_REQUIRED` (exit 3) and the digest to pass. The digest is the SHA-256 of the canonical JSON of the decision: trial id, tool, adapter, package, the install and its dependencies or the pre-existing owners, every baseline path with `existed` and `sha256`, the unknown tokens, the binary identity and the binding stamp. A changed provider answer, candidate, binary or binding changes it.
4. **Under the lock.** The writer lock is taken. The identity and the ledger are checked, the providers are probed again and the baseline is observed again. A digest that differs now is `E_PLAN_STALE` (exit 3, nothing written). A trial record of the same id in `state/trials/` is `E_CONFLICT` (exit 3). This is checked before the lock and again by the exclusive create, so two concurrent runs cannot replace each other's record.
5. **The record first.** The trial record is created (`RENAME_EXCL`, never replacing) before the provider changes anything. Its `package.install.op_id` is the journal operation id of the install, so the record, the journal and the receipt share one id.
6. **The install.** For a missing tool: the `intent` journal entry (no path), the provider's install (`brew install homebrew/core/<name>`, `brew install --cask <name>`, `mise install <tool>@latest` or `mise install <tool>@<version>`, docs/packages.md), a probe as the postcondition (`Installed`, else `failed`), and the `done` or `failed` entry. An outcome entry that cannot be written (for example, a journal at its cap) makes the operation `failed` with both causes in its error. The receipt and the result line still follow, and the exit is 1, because the record was already written. A pre-existing tool journals nothing.
7. **The receipt.** `command = trial`, `op_id` as above, `plan_digest` and a `yes_plan` approval, and one operation (`updated`, `current` or `failed`) whose `id` is the journal id. The output ends with `trial: state/trials/<id>.json`, `receipt: <name>` and `result:`.

| Exit | When |
| --- | --- |
| 0 | The trial is recorded (installed, or pre-existing). |
| 1 | The install failed after the record was written. The record and the `failed` entry stay. |
| 2 | Usage. |
| 3 | Blocked before any write: approval missing, ambiguous provider, no capable provider, baseline failure, stale digest, existing trial, lock held, not bound. |
| 130, 143 | A stop request. The provider child is ended (docs/secrets.md, "How `op` is run"). |

## The record

`state/trials/<trial-id>.json`, schema 2 (schema 3 once adopted), canonical JSON. The docs/state.md validation applies: identity stamp, valid id, claim check on every path, secrets refused.

| Field | Meaning |
| --- | --- |
| `trial_id`, `tool`, `binding`, `created_unix_ns` | `tool` is the short name. |
| `baseline.tool_preexisting` | True when a provider listed the tool or another owner had it. |
| `baseline.owners` | The owners found when it pre-existed: `<adapter> (<version>)` per listing provider, or the other-owner text. Left out when empty. |
| `baseline.paths[{path, existed, sha256}]` | The candidates observed before the install. `sha256` is present for a regular file only. |
| `owned_outputs` | Empty. The trial writes no configuration. |
| `package.adapter`, `package.name` | The provider that lists or installed the tool, and the package as it names it. Left out when only another owner has the tool. The journal and receipt then name the adapter `other-owner`. |
| `package.install{op_id, dependencies}` | Present only when the trial installed: the journal and receipt id, and the dependencies the provider added. |
| `unknown` | Fixed tokens `services`, `paths-outside-baseline`, `provider-caches`: effects the trial does not instrument. Candidates for review, never owned cleanup scope. |
| `adopted{op_id, module, at_unix_ns}` | Set once by `adopt` (DFM-33), with `schema` 3. It names the journal operation and the module that took the configuration. Left out of an active trial. |

The optional fields `package` and `unknown` need no schema bump: a record without them (for example, one the test harness `__record-trial` writes) still reads. `adopted` is different: the record type refuses unknown keys, so an adopted record is schema 3 and an unadopted record stays schema 2. The validator accepts 2 and 3, refuses `adopted` on schema 2, and requires it on schema 3. A binary that predates schema 3 reports an adopted record as `E_STATE_NEWER`.

A trial is **active** until its record has `adopted`. An adopted record stays as evidence and keeps its id taken (a second trial of that id is `E_CONFLICT`). Readers that mean "in progress" skip it: `doctor` lists no row for it, and `packages --update` does not roll its tool from the record. `state check` still validates and counts it.

The record is a private file under `state/trials/`. `state check` validates it with the other documents and counts it (`trials: N (M unreadable)`). `doctor` and `status` show an unreadable or foreign-identity record as a WARN row. An interrupted install follows the packages rule (docs/packages.md, "Interrupted install"): `state check` reports `E_OP_INTERRUPTED`, `state recover` leaves it `unknown`, and `state resolve <op_id> --as failed` settles it.

## Updates

A trial is updated with the other packages as if it were already adopted (the owner's decision of 2026-10-31). Nothing holds an update for a trial.

- **Homebrew.** `sync`'s `brew` updater upgrades a trial's formula or cask with every other outdated package (docs/upgrade.md, "Trials").
- **mise.** `dfm packages --update` rolls the tool of each active `mise/v1` trial as a declared rolling tool (docs/toolchains.md, "Trials"). `sync` runs `packages` without `--update`, so it rolls no mise tool, declared or trial.

The record is not changed by an update. It keeps what the trial saw at its start.

## Decisions

| Question | Choice |
| --- | --- |
| Command name | `trial`, the noun the state store uses for the record. DESIGN.md section 7 proposes `try`. |
| Ambiguous choices without a prompt | No command has a prompt. An ambiguous provider is `E_APPROVAL_REQUIRED` with `--provider` as the fix. A path choice is never ambiguous: the candidates are explicit, or one default. |
| Decline | The absence of `--yes-plan`: exit 3 `E_APPROVAL_REQUIRED`, as for `apply`. Exit 4 (`declined`) is reserved for an interactive prompt, which does not exist. |
| Existing configuration | The trial changes no configuration. A pre-existing candidate is recorded with its hash (`existed: true`), and the cleanup scope line says it is never removed. The tool's own first run may change it. The baseline hash attributes that change. |
| Configuration discovery | The providers do not know configuration paths. Without `--config`, the one candidate is `~/.config/<tool>` (a `shortcut:` in `candidates()`). |
| Native isolated configuration | Not used. No adapter declares one, and the trial does not start the tool. |
| Dependencies | Recorded in `package.install.dependencies` from the provider's own listing (brew only; mise lists none). A dependency may be shared, so purge must decide per dependency. |
| Pre-existing tool | Still recorded (baseline, owners, candidates) with a `current` receipt, so `adopt` can later promote its configuration. |
| Second trial of a tool | Refused (`E_CONFLICT`) while the record exists, also when its install was interrupted. `--id` names a second trial. |
| Receipt for a crash | None. A crash after the record or the intent leaves the record, the journal and `state check`'s finding. Only a run that reaches its end writes a receipt. |
| Signal handlers | Armed once, before the first probe. The gate test in `crates/cli/tests/gate.rs` lists `trial_cmd.rs` beside `apply_cmd.rs` as the only callers. |

## Known limits

- **Not an OS sandbox.** Services, caches, logs and any path outside the baseline are declared `unknown`. A path the tool creates there is a candidate for review at purge time, never an owned cleanup scope, and never gets a retroactive baseline.
- **One candidate by default.** A tool that keeps its configuration elsewhere needs `--config`. The record cannot be extended after the fact without a new baseline, so pass every candidate on the first run.
- **The baseline is one observation.** A hard-linked file is hashed like any other. `purge` must check again before it deletes.
- **One end of a trial.** `adopt` ends a trial ([docs/adopt.md](adopt.md)). `purge` is not built, so a trial that is not adopted stays until it is removed by hand.
- **Rerun cost.** Two probe rounds (before and under the lock), plus the postcondition probe after an install.
- **Evidence scope.** Tests use the `test-isolation` build and the `brew` and `mise` look-alikes. No test runs a real install. Whether `sync` keeps active trials intact is not tested.

## What adopt and purge inherit

- **Attribution by one id.** `package.install.op_id` names the journal `intent` and `done` (or `failed`) entries and the receipt `<start>-<op_id>.json`. A trial without `package.install` installed nothing. An `op_id` with no `intent` entry means the run ended between the record and the intent: nothing was started, and purge treats the package as not trial-installed until the provider's listing says otherwise.
- **Baseline for restore.** `baseline.paths` with `existed` and `sha256` as observed before the install. `existed: false` is not delete authority: nothing observes the path after the install, so a file created there by anything else is indistinguishable from the tool's. Purge treats such a path as a candidate that needs its own approval (its preview shows the live hash and the approved digest covers it). It restores nothing for a pre-existing path whose hash no longer matches, and never touches a path with `existed: true` and no hash (a directory).
- **The installed version.** Purge removes the trial's package by name (`package.name`), whatever version is installed, because updates may change it after the trial starts (docs/upgrade.md, "Trials"). The record keeps what it saw at trial start as history and is not updated.
- **Owners.** `baseline.owners` says who had the tool. A pre-existing tool is never uninstalled. A dependency in `package.install.dependencies` is uninstalled only when no module or other trial needs it.
- **Unknown effects.** Purge reports the `unknown` entries as retained or unattributed, never as cleaned.
- **Candidates for adopt.** `baseline.paths` are the files adopt reviews and promotes into a module. `package.adapter` and `package.name` are the install declaration it generates.
