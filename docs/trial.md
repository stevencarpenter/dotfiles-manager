# Trial (DFM-32)

Task: DFM-32, the `trial` command (`try <tool>` in DESIGN.md section 7). This document records what the first trial does and where it resolves or differs from [docs/architecture.md](architecture.md) (the contract) and DESIGN.md "Trial, adopt, or purge". The contract wins where this document is silent. The package providers it reuses are in [docs/packages.md](packages.md), the state store in [docs/state.md](state.md). `adopt` (DFM-33) and `purge` (DFM-34) are not built; the last section lists what they inherit.

## What exists

| Part | Location |
| --- | --- |
| `trial` command: arguments, owner discovery, baseline, digest, the install and the receipt | `crates/cli/src/trial_cmd.rs` |
| Trial record fields `baseline.owners`, `package{adapter, name, install{op_id, dependencies}}`, `unknown` | `crates/model/src/state.rs` (`TrialRecord`, `TrialPackage`, `TrialInstall`) |
| Record write (atomic replace, identity, claim and secret checks) and load | `crates/engine/src/store.rs` (`write_trial`, `load`), unchanged |
| Provider probes and installs (`homebrew/v1`, `mise/v1`) | `crates/cli/src/adapters/package.rs`, unchanged |
| Tests with `brew` and `mise` look-alikes | `crates/cli/tests/trial.rs`, scripts in `crates/cli/tests/common/providers.rs` |

## The command

```
dfm trial <package> [--id <trial-id>] [--provider homebrew/v1|mise/v1] [--config <path>]... [--preview] [--yes-plan <digest>]
```

`<package>` is the package as the provider names it (`atuin`, `homebrew/core/atuin`, `node@20`), checked by the package adapter's name rule. The tool is its short name (`atuin`, `node`), which must be a valid id (lowercase ASCII letters, digits, `.`, `_`, `-`); a mise backend name such as `npm:prettier` is a usage error (known limit). The trial id is `trial-<tool>` unless `--id` names another.

Order of one run:

1. **Owner discovery (C01).** The signal handlers go in, then the providers are probed read-only exactly as `packages` probes them (Homebrew's Cellar and Caskroom, `mise ls --installed --json`, the other-owner check by executable name, no tap code, auto-update and mise configuration off). With `--provider` only that provider is asked. Without it both are asked:
   - A provider lists the package, or an executable of its name has another owner: the tool **pre-exists**. Its owners are recorded (`homebrew/v1 (18.24.1)`, or the other-owner text with its path) and nothing is installed.
   - Exactly one provider reports it missing and installable: that provider installs it, with the dependencies the provider names.
   - More than one can install it: the choice is the owner's, `E_APPROVAL_REQUIRED` (exit 3) with the candidates and `--provider` as the fix. Nothing else is asked; every other choice has one answer.
   - None can: a blocker with the probe's own code (`E_PROBE_FAILED` when any provider could not answer, else `E_SOURCE_UNTRUSTED`, `E_PLATFORM_UNSUPPORTED`, `E_CONFLICT` or `E_PRIVILEGE_UNSUPPORTED` as docs/packages.md maps them) and each provider's answer.
2. **Baseline (C02).** The configuration candidates are the `--config` paths (absolute, or relative to HOME), or `~/.config/<tool>` when none is given. Each must pass the store's claim check (inside HOME, outside the manager's own paths) before any provider runs. Each is observed once: a regular file is hashed (`file_fingerprint`, the value an observation records), a directory and an absent path are recorded as such. A link, a special file, an unreadable entry or a file over 16 MiB is a **baseline failure**: `E_STATE_INVALID`, exit 3, nothing written, because recording it as absent would let a later purge delete what was there.
3. **Preview.** The decision is printed: the trial and tool, the provider and package, the action (install with its dependencies, or nothing because the tool pre-exists and who owns it), one `baseline:` line per candidate, the cleanup scope, the `unknown:` effects and `plan digest:`. `--preview` stops here, exit 0 (3 when blocked). Without `--yes-plan` the run stops with `E_APPROVAL_REQUIRED` (exit 3) and the digest to pass. The digest is the SHA-256 of the canonical JSON of that decision: trial id, tool, adapter, package, the install and its dependencies or the pre-existing owners, every baseline path with `existed` and `sha256`, the unknown tokens, the binary identity (version and SHA-256 of the running executable) and the binding stamp, as a plan digest is. A changed provider answer, a changed candidate, another binary or another binding changes it.
4. **Under the lock.** The writer lock is taken, the identity and the ledger are checked, the providers are probed again and the baseline is observed again. A digest that differs now is `E_PLAN_STALE` (exit 3, nothing written). A trial record of the same id already in `state/trials/` is `E_CONFLICT` (exit 3): checked before the lock, and again by the exclusive create below, so two runs that both passed the first check cannot replace each other's record.
5. **The record first.** The trial record is created (`RENAME_EXCL`, never replacing, docs/state.md rules) before the provider changes anything. Its `package.install.op_id` is the journal operation id the install will use, so the record, the journal and the receipt are joined by one id.
6. **The install.** For a missing tool: the `intent` journal entry (no path: the provider's listing is the record, as for `packages`), `brew install homebrew/core/<name>` or `mise install <name>` through the bounded runner, a probe as the postcondition (`Installed`, else the operation is `failed`), the `done` or `failed` entry. An outcome entry that cannot be written (for example a journal at its cap) makes the operation `failed` with both causes in its error, as the engine records a package install; the receipt and the result line still follow, and the exit is 1, never 3, because the record was already written. A pre-existing tool journals nothing.
7. **The receipt.** `command = trial`, `op_id` as above, `plan_digest` and a `yes_plan` approval, one operation (`updated`, `current` or `failed`) whose `id` is the journal id. Then one line per step, `trial: state/trials/<id>.json`, `receipt: <name>` and `result:`.

Exit codes: 0 when the trial is recorded (installed, or pre-existing); 1 when the install failed after the record was written (the record and the `failed` entry stay); 2 usage; 3 blocked before any write (approval missing, ambiguous provider, no capable provider, baseline failure, stale digest, existing trial, lock held, not bound); 130 or 143 on a stop request (the provider child gets SIGTERM, then SIGKILL after 10 s, as docs/packages.md describes).

## The record

`state/trials/<trial-id>.json`, schema 2, canonical JSON (docs/state.md validation applies: identity stamp, valid id, claim check on every path, secrets refused):

| Field | Meaning |
| --- | --- |
| `trial_id`, `tool`, `binding`, `created_unix_ns` | As before DFM-32. `tool` is the short name. |
| `baseline.tool_preexisting` | True when a provider listed the tool or another owner had it. |
| `baseline.owners` | The owners found when it pre-existed: `<adapter> (<version>)` per listing provider, or the other-owner text. Left out when empty. |
| `baseline.paths[{path, existed, sha256}]` | The candidates observed before the install. `sha256` is present for a regular file only. |
| `owned_outputs` | Empty. The trial writes no configuration. |
| `package.adapter`, `package.name` | The provider that lists or installed the tool and the package as it names it. Left out when only another owner has the tool (an executable of its name outside both providers): no provider owns anything then, and the journal and receipt name the adapter `other-owner`. |
| `package.install{op_id, dependencies}` | Present only when the trial installed: the journal and receipt id, and the dependencies the provider added with the package. Left out when the tool pre-existed. |
| `unknown` | Fixed tokens `services`, `paths-outside-baseline`, `provider-caches`: effects the trial does not instrument. Candidates for review, never owned cleanup scope (C04). |

A record written by the test harness (`__record-trial`) or before this change has none of the optional fields and still reads; nothing bumped the schema. The reverse does not hold: a binary built before DFM-32 refuses unknown fields, so it reports a record this command wrote as unreadable (`E_STATE_INVALID` in `state check`, a WARN row in `doctor`). That is intended: a schema bump would refuse every state document as `E_STATE_NEWER` instead, and the installed binary is rebuilt from the merged source before a trial runs on a live machine.

Durability (C03): the record is a private file under `state/trials/`, visible after the process ends. `state check` validates it with the other documents and counts it (`trials: N (M unreadable)`); `status` and `doctor` count and report it through the existing evidence rows (an unreadable or foreign-identity record is a WARN row; a readable one is not listed per trial yet). An interrupted install leaves the `intent` entry open: `state check` reports `E_OP_INTERRUPTED`, `state recover` leaves it `unknown` (the intent has no path to probe), and `state resolve <op_id> --as failed` settles it.

## Decisions where the documents were silent

| Question | Choice |
| --- | --- |
| Command name | `trial`, the noun the state store, README and DESIGN.md use for the record. DESIGN.md section 7 proposes `try`; README says the command names are proposals. Renaming is a one-line change in `lib.rs`. |
| "Ask only for ambiguous choices" without a prompt | No command has a prompt (architecture section 10, noninteractive). An ambiguous provider is `E_APPROVAL_REQUIRED` with `--provider` as the fix; a path choice is never ambiguous because the candidates are explicit or one default. |
| Decline | The absence of `--yes-plan`: exit 3 `E_APPROVAL_REQUIRED`, as `apply`, `secrets` and `packages`. Exit 4 (`declined`) is reserved for an interactive prompt that does not exist; no `Code` maps to it yet. |
| "Ask before changing existing configuration" | The trial changes no configuration. A pre-existing candidate is recorded with its hash (`existed: true`) and the cleanup scope line says it is never removed. The tool's own first run may change it; that is the tool's effect, attributed by the baseline hash. |
| Configuration discovery | The providers do not know configuration paths. Without `--config`, the one candidate is `~/.config/<tool>`. `shortcut:` in `candidates()`; upgrade when an adapter can declare paths. |
| Native isolated configuration | Not used: no provider or tool adapter declares one, and the trial does not start the tool. Recorded here as not built, not claimed. |
| Dependencies | Recorded in `package.install.dependencies` from the provider's own listing (brew only; mise lists none). A dependency may be shared, so purge decides per dependency. |
| Trial id and tool for a name with `/` or `@` | The tool is the short name; the exact package name is in `package.name`. |
| Pre-existing tool | The trial is still recorded (baseline, owners, candidates) with a `current` receipt, so `adopt` can later promote its configuration. |
| Second trial of a tool | Refused (`E_CONFLICT`) while the record exists, including a record whose install was interrupted. `--id` names a second one; `purge` is the way to end one. |
| Receipt for a crash | None. A crash after the record or the intent leaves the record, the journal and `state check`'s finding; a receipt is written only by a run that reaches its end. |
| Signal handlers | Armed once, before the first probe, as `packages` does. The gate test in `crates/cli/tests/gate.rs` lists `trial_cmd.rs` as the second caller. |

## Known limits

- **Not an OS sandbox.** Services, caches, logs and any path outside the baseline are declared `unknown`. A path the tool creates there is a candidate for review at purge time, never an owned cleanup scope, and never gets a retroactive baseline (C04).
- **One candidate by default.** A tool that keeps its configuration elsewhere needs `--config`. The record cannot be extended after the fact without a new baseline, so pass every candidate on the first run.
- **The baseline is one observation.** A hard-linked file is hashed like any other; the harness's one-link rule is not applied here, because the trial restores nothing. `purge` must check again before it deletes.
- **No trial row in `doctor`.** `doctor` and `status` count trials and report unreadable ones; a per-trial row with its owner and age is not built (`doctor_cmd.rs` was not changed).
- **Rerun cost.** Two probe rounds (before and under the lock), plus the postcondition probe after an install, as `packages`.
- **Evidence scope.** Tests use the `test-isolation` build and the `brew` and `mise` look-alikes. No real install ran. `sync` (DFM-30) and `update` (DFM-31) must prove that they preserve active trials when they exist (C05, second sentence).

## What adopt (DFM-33) and purge (DFM-34) inherit

- **Attribution by one id.** `package.install.op_id` names the journal `intent` and `done` (or `failed`) entries and the receipt `<start>-<op_id>.json`. A trial without `package.install` installed nothing. An `op_id` with no `intent` entry means the run ended between the record and the intent (a crash, or a journal that refused the entry): the install is unknown, nothing was started, and purge treats the package as not trial-installed until the provider's listing says otherwise.
- **Baseline for restore.** `baseline.paths` with `existed` and `sha256` as observed before the install. `existed: false` is not delete authority: nothing observes the path after the install, so a file created there by anything else before the purge is indistinguishable from the tool's. Purge treats such a path as a candidate that needs its own approval (its preview shows the live hash and the approved digest covers it), never as owned scope. It restores nothing for a pre-existing path whose hash no longer matches, and never touches a path with `existed: true` and no hash (a directory).
- **Owners.** `baseline.owners` says who had the tool; a pre-existing tool is never uninstalled, and a dependency in `package.install.dependencies` is uninstalled only when no module or other trial needs it.
- **Unknown effects.** `unknown` lists what was not instrumented. Purge reports them as retained or unattributed, never as cleaned.
- **Candidates for adopt.** `baseline.paths` are the files adopt reviews and promotes into a module; `package.adapter` and `package.name` are the install declaration it generates.
