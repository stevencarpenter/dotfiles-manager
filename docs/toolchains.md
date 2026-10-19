# Toolchains (DFM-24)

Task: DFM-24, the rolling update of runtime and CLI tools that `mise/v1` and `uv-tool/v1` own. This document records what `packages --update` does, the interface `sync` (DFM-30) and `update` (DFM-31) compose, and where the implementation resolves or differs from [docs/architecture.md](architecture.md) (the contract) and DESIGN.md sections 7 and 8. The contract wins where this document is silent. The install path of the same adapters is in [docs/packages.md](packages.md).

## What exists

| Part | Location |
| --- | --- |
| `PackageState::Rolling`, `PackageState::Held`, `Action::Upgrade` and their plan rows (`Planner::package`) | `crates/model/src/plan/` |
| `maintain` (the update view of a probe), `upgrade`, `update_facts`, the `uv-tool/v1` probe and install | `crates/cli/src/adapters/package.rs` |
| Execution (`try_install` runs an `upgrade` as it runs an `install`: re-probe, intent, provider, re-probe, outcome; the listing before and after decides `updated` or `current`) | `crates/engine/src/apply.rs` |
| `packages --update` | `crates/cli/src/apply_cmd.rs`, `crates/cli/src/lib.rs` |
| Tests with `mise`, `uv` and `brew` look-alikes | `crates/cli/tests/toolchains.rs` |

## The inventory

The personal MacBook declares its runtimes and CLI tools in `~/.config/mise/config.toml` and `conf.d/*.toml` (go, java, scala, terraform, kubectl and others as version prefixes; most CLI tools, among them `npm:` and `go:` backends, as `latest`) and installs a few Python tools with `uv tool install` from local paths and Git origins. `mise` and `uv` themselves come from Homebrew. Both providers keep every installed version until told otherwise; mise selects a version per directory from its configuration, and uv keeps one version per tool.

Two provider paths are built: mise (runtimes and tools from mise's registry and backends, by the name mise takes) and uv tools by package-index name. A uv tool from a Git origin or a local path is not built: it needs origin trust (architecture section 8, trust) and a source identity the manager can record, which is a different contract from a package name. Declared, it is refused as `E_MANIFEST_INVALID`; installed that way by hand (uv records the source in `~/.local/share/uv/tools/<name>/uv-receipt.toml`, from which `uv tool upgrade` resolves the tool again), it is held under `--update`. Both stay with the scripts that install them today; the path is recommended as its own task.

## The command

```
dotfiles-manager packages --root <dir> --update [--preview] [--yes-plan <digest>]
```

`packages --update` is `packages` with a rolling update as the decision for each installed tool. Everything else is as in docs/packages.md: the providers are probed read-only before the plan, the signal handlers go in before the first probe, the plan prints with its digest, `--yes-plan` approves it, the probes run again under the lock, and each operation is journaled. A plain `packages` digest does not approve an update: the plan rows and their preconditions differ, so the digest differs, and the mismatch is `E_PLAN_STALE`. `--update` with `secrets` or `apply` is a usage error.

## One tool under `--update`

The probe is the one docs/packages.md describes (`mise ls --installed --json <tool>`, `uv tool list`), with the same provider variables, so a listing never loads a configuration file, answers a prompt, installs a tool or reaches the network. Then, for a tool the provider lists:

| Declaration and state | Plan row | Apply |
| --- | --- | --- |
| `update_policy = "rolling"`, mise or uv, no other owner, no local build | `planned`, action `upgrade`, precondition `probe:<adapter>:<name> = rolling:<installed versions>`, effect "upgrade `<name>` with `<adapter>` to the newest version the provider resolves at apply time; the installed `<versions>` stay" | `mise install <tool>@latest` (a bare name) or `mise install <tool>@<prefix>` (a declared prefix: the newest that matches); each is a no-op when that version is installed. Or `uv tool upgrade <name>`. Then the listing again. A changed listing is `updated`, the same listing is `current`. |
| `update_policy` of `hold` or `native-request` (another value is `E_MANIFEST_INVALID` in every mode) | `held`, "held (`<versions>`, update_policy = `<policy>`); not upgraded" | nothing |
| a uv tool whose receipt (`~/.local/share/uv/tools/<name>/uv-receipt.toml`, `[tool].requirements`) names a `git`, `path`, `directory`, `url` or `editable` source, or whose receipt cannot be read | `held`, "held (`<version>`, installed from a `<key>` source, which an upgrade would resolve again outside the plan)", or the read error | nothing |
| a uv tool whose receipt writes a requirement as one PEP 508 string (`"pkg @ git+https://..."`, uv's legacy form) instead of a table, even a bare name | `held`, "held (`<version>`, installed with a legacy string requirement in the receipt, whose source an upgrade would resolve again outside the plan)" | nothing |
| a uv tool whose receipt has a non-empty `[tool.options]` table (uv persists `--index-url`, `--default-index`, `--index`, `--extra-index-url`, `--find-links`, `--no-index`, `--prerelease`, `--resolution`, `--exclude-newer`, `--compile-bytecode` and the other install settings there, and `uv tool upgrade` reuses them) | `held`, "held (`<version>`, installed with uv options (`<keys>`), which an upgrade would keep outside the plan)" | nothing |
| a mise version of the tool that is a local build (`ref:`, `path:`), inside or outside the declared prefix (`go@1.27` with `ref:feature` installed) | `held`, "held (`<versions>`, `<version>` is a local build, which is kept)" | nothing |
| a mise declaration whose version the listing contains as it is (`node@20.20.2` with `20.20.2` installed) | `held`, "held (`<versions>`, an exact pin resolves to itself)" | nothing |
| `homebrew/v1` or `apt/v1` | `held`, "held (`<versions>`, `<adapter>` has no rolling update path in this build)" | nothing |
| an executable of the tool's name in another owner's directory (for uv, that includes a file or a foreign link at `~/.local/bin/<name>`; only a link into `~/.local/share/uv/tools` is uv's own) | a blocker, `E_CONFLICT`: "declared for `<adapter>` but an executable `<path>` exists; a declared owner is not permission to replace another owner" | not approved |

A tool the provider does not list is planned as docs/packages.md plans it: `install`, with the same owner check. A probe that cannot run is `E_PROBE_FAILED`; a bad name is `E_MANIFEST_INVALID`.

**What is never run.** `mise prune`, `mise uninstall`, `mise upgrade`, `mise use`, `mise self-update`, `uv tool uninstall`, `uv self update`, and `uv tool upgrade` of a tool whose receipt names a Git, path, URL or editable source (the test asserts that every provider line of a rolling update is a listing, `mise install` or `uv tool upgrade`). No version is removed and no configuration file is written, so every installed version, a project's `.mise.toml` or `.tool-versions`, and mise's selection for the current directory stay as they were. The `uv` changes run with `UV_PYTHON_DOWNLOADS=never`: a tool whose interpreter is missing fails (`failed`, journaled) instead of installing a Python the plan does not show.

**Identities.** The plan row reports the declared identity (`name`, `update_policy`, the adapter) and the installed identity (every version the provider lists for the name, in the provider's order; a prefix such as `go@1.27` lists the versions that match). The selected identity is the provider's: mise selects per directory from its configuration, which the manager does not evaluate before approval, and uv's one version is the selected one. A mise shim ahead of another copy on the search path is not reported as a broken installation; only an executable in another owner's directory is, and under `--update` only, as a blocker.

## The interface for `sync` and `update`

- `sync` (declared pins): `crate::adapters::package::package_facts(resolved, runner)` and the planner's packages scope. A declared `tool@version` is installed when no listed version matches it (docs/packages.md, version prefix matching); a changed pin installs the new version beside the old one; nothing is upgraded, downgraded or removed.
- `update` (rolling maintenance): `crate::adapters::package::update_facts(resolved, runner)`, the same planner scope, and `Action::Upgrade` in the engine. The adapter operations are `package::maintain` (the probe in the update view) and `package::upgrade` (the provider change). The engine's `Generator::probe` and `Generator::install` dispatch on `Action::Upgrade`.
- Both: `packages` and `packages --update` are the whole package part of those commands, with their gates, lock, journal and receipt.

## Upgrading mise or uv themselves

The manager is a static binary that needs neither mise nor uv at run time (architecture section 2), so a change to either cannot stop the coordinator or the recovery path: `state recover`, `state resolve` and `doctor` read the journal and receipts, not a provider. Both providers come from Homebrew on the inventoried machines, and `homebrew/v1` has no rolling update path in this build, so neither is upgraded by the manager. A tool mise installs is never mise, and a tool uv installs is never uv, under this adapter; `mise self-update` and `uv self update` are not run. The durable order for every upgrade is the one docs/state.md fixes: the `intent` entry (`fsync`) before the provider starts, the provider change, the listing again, the `done` or `failed` entry, then the receipt. An interruption leaves the `intent` without an outcome; the resource blocks with `E_OP_INTERRUPTED` in every later run until `state resolve <journal-id> --as failed`, and the installed versions, old and new, stay listed (test `an_interrupted_upgrade_keeps_the_installed_versions_and_blocks_until_resolved`).

## Decisions where the documents were silent

| Question | Choice |
| --- | --- |
| Command shape | A `--update` flag on `packages`, not an `update` command: `update` (DFM-31) composes sources, locks and every kind; this is its package part, callable and testable now. |
| Candidate version | Not shown in the plan. Discovering it needs the provider's registry (`mise latest`, uv's index), which is the network, and a plan is read-only. The provider resolves it at apply time after approval; the listing before and after is compared and the next plan shows the installed set. DESIGN.md section 7 item 1 asks for the eligible candidate before mutation; this is the recorded gap, to close when `update` gets a network phase from trusted origins. |
| What `rolling` means for a pin | A declared `tool@version` is a prefix: `mise install tool@1.27` resolves the newest `1.27.x`. A declared version the listing contains as it is (`node@20.20.2` with `20.20.2` installed) is an exact pin: `mise install node@20.20.2` is a no-op, so the row holds the tool instead of planning an upgrade that cannot advance. The rule is the listing, not the number of components, because the listing format differs by backend. Advancing a pin is a manifest edit (the lock has no package pin; DFM-31). |
| Policy values | `rolling`, `hold` or `native-request`; only `rolling` rolls, the other two hold with the value in the row. Any other value is `E_MANIFEST_INVALID` in both modes, so a typo is refused instead of holding silently. |
| A copy from another owner | Reported and left alone under `packages` when the declared provider lists the tool (it is not a broken installation), a blocker under `--update` (an upgrade would deepen the duplicate ownership). The check is by executable name in the other owners' directories (docs/packages.md, One owner); the mise shims are mise's own. uv links its entry points into `~/.local/bin`, which other installers use too, so there only a link whose target is under `~/.local/share/uv/tools` is uv's; a file or another link at that name is a foreign owner the upgrade would link over. |
| Local builds | A mise version starting with `ref:`, `ref-`, `path:` or `path-` holds the tool. The check reads the whole listing of the tool before the declared prefix narrows it, so `go@1.27` is held by an installed `ref:feature`. The spellings `ref-` and `path-` are a guess at mise's listing and are marked as a shortcut in the code. |
| `mise install` as the upgrade | Chosen over `mise upgrade`, which reads and can edit mise's configuration, and over `mise latest` plus `mise install`, which is two network calls. The selector is always explicit: `mise install <tool>` with a bare name installs only the version mise's configuration requests, and `MISE_NO_CONFIG=1` hides that configuration, so a bare declaration is sent as `<tool>@latest` and a declared prefix as `<tool>@<prefix>`; each resolves the newest matching version and is a no-op when it is installed. The look-alike in the test refuses a bare name. |
| `uv tool list` names | Compared as uv normalizes them (PEP 503: lowercase, each run of `_`, `.` and `-` as one `-`, so `Friendly__Bard`, `friendly--bard` and `friendly.bard` are `friendly-bard`); the first word of a `<name> v<version>` line, so a source note after the version does not break the read. The receipt directory is looked up by the same name. |
| uv receipt forms | Only a receipt whose requirements are all tables without a source key, and with no `[tool.options]` entry, rolls. uv once wrote a requirement as one PEP 508 string; its source is not visible to the table keys, so any string requirement holds, a bare name too (a conservative hold is the rule). uv persists index, find-links, prerelease, resolution and build settings there and `uv tool upgrade` reuses them, so an upgrade of such a tool could fetch from an index the plan never showed. The hold names the keys; a harmless key (`compile-bytecode`) holds too, because an allow list of uv's options would need upkeep with every uv release. |
| Receipt | `command = packages` for both modes, with `update = true` for a rolling update (a receipt written before the field reads as `false`). An update that changed nothing still writes a receipt, because the provider was asked (the journal holds its `intent` and `done`). |
| `homebrew/v1` under `--update` | Held, with the reason in the row, not a blocker: a module that mixes brew and mise tools can still run `packages --update` for the mise ones. A rolling Homebrew upgrade is its own task (it upgrades dependents and needs its own owner check). |

## Known limits

- **No candidate in the preview.** The reviewer approves "the provider's newest" without seeing the version (above).
- **Selected identity.** The plan reports declared and installed; which version a shim selects in a given directory is mise's decision from configuration the manager does not evaluate. `doctor` does not probe tools yet.
- **Git and path sources, uv options.** A uv tool from a Git origin or a local path, a uv tool installed with options, and a mise `ref:` or `path:` declaration, are refused or held; the scripts that install them today keep that job.
- **mise backends.** A tool of a mise backend (`npm:`, `go:`, `github:`, `ubi:`, `vfox:`, and the others) resolves its newest version from that backend's origin under `--update`, with the same trust as the existing install path of docs/packages.md: the backend's origin, not a plan row, decides what is fetched.
- **`mise ls` without configuration.** The listing shows every installed version and no `active` or `requested_version` field; the adapter does not read those.
- **Homebrew and apt.** No rolling update path; both are held under `--update`.
- **Evidence scope.** Tests use the `test-isolation` build and `mise`, `uv` and `brew` look-alikes. `mise ls --json`, `mise --version` and `uv tool list` were run by hand against the real tools once to learn their output; no real install or upgrade was run by a test.
