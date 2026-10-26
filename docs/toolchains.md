# Toolchains

`packages --update`: the rolling update of runtime and CLI tools that `mise/v1` and `uv-tool/v1` own. The install path of the same adapters, their probes and provider variables are in [docs/packages.md](packages.md). [docs/architecture.md](architecture.md) is the contract and wins where this document is silent.

| Part | Location |
| --- | --- |
| `PackageState::Rolling`, `PackageState::Held`, `Action::Upgrade` and their plan rows (`Planner::package`) | `crates/model/src/plan/` |
| `maintain` (the update view of a probe), `upgrade`, `update_facts` | `crates/cli/src/adapters/package.rs` |
| Execution (`try_install` runs an `upgrade` as it runs an `install`) | `crates/engine/src/apply.rs` |
| `packages --update` | `crates/cli/src/apply_cmd.rs`, `crates/cli/src/lib.rs` |
| Tests with `mise`, `uv` and `brew` look-alikes | `crates/cli/tests/toolchains.rs` |

Two provider paths are built: mise (runtimes and tools from mise's registry and backends, by the name mise takes) and uv tools by package-index name. A uv tool from a Git origin or a local path is not built. It needs origin trust (architecture section 8) and a source identity the manager can record. Declared, it is `E_MANIFEST_INVALID`. Installed that way by hand, it is held under `--update`.

## The command

```
dfm packages [--root <dir>] --update [--preview] [-y|--yes-plan <digest>]
```

`packages --update` is `packages` with a rolling update as the decision for each installed tool. Everything else is as in docs/packages.md: read-only probes before the plan, signal handlers before the first probe, the digest, the probes again under the lock, and one journaled operation per tool. A plain `packages` digest does not approve an update: the plan rows and preconditions differ, so the digest differs (`E_PLAN_STALE`). A rolling tool is always `planned`, so an update run with one needs `--yes-plan`. `--update` on any other command is a usage error.

## One tool under `--update`

The probe is the one docs/packages.md describes, with the same provider variables. A listing never loads a configuration file, answers a prompt, installs a tool or reaches the network. For a tool the provider lists:

| Declaration and state | Plan row | Apply |
| --- | --- | --- |
| `update_policy = "rolling"`, mise or uv, no other owner, no local build | `planned`, action `upgrade`, precondition `probe:<adapter>:<name> = rolling:<installed versions>`, effect "upgrade `<name>` with `<adapter>` to the newest version the provider resolves at apply time; the installed `<versions>` stay" | `mise install <tool>@latest` (a bare name) or `mise install <tool>@<prefix>` (a declared prefix), each a no-op when that version is installed. Or `uv tool upgrade <name>`. Then the listing again: a changed listing is `updated`, the same listing is `current`. |
| `update_policy` of `hold` or `native-request` | `held`, "held (`<versions>`, update_policy = `<policy>`); not upgraded" | nothing |
| A uv tool whose receipt (`~/.local/share/uv/tools/<name>/uv-receipt.toml`, `[tool].requirements`) names a `git`, `path`, `directory`, `url` or `editable` source, or whose receipt cannot be read | `held`, "held (`<version>`, installed from a `<key>` source, which an upgrade would resolve again outside the plan)", or the read error | nothing |
| A uv tool whose receipt writes a requirement as one PEP 508 string (uv's legacy form), even a bare name | `held`, "held (`<version>`, installed with a legacy string requirement in the receipt, whose source an upgrade would resolve again outside the plan)" | nothing |
| A uv tool whose receipt has a non-empty `[tool.options]` table | `held`, "held (`<version>`, installed with uv options (`<keys>`), which an upgrade would keep outside the plan)" | nothing |
| A mise version of the tool that is a local build (`ref:`, `path:`), inside or outside the declared prefix | `held`, "held (`<versions>`, `<version>` is a local build, which is kept)" | nothing |
| A mise declaration whose version the listing contains as it is (`node@20.20.2` with `20.20.2` installed) | `held`, "held (`<versions>`, an exact pin resolves to itself)" | nothing |
| `homebrew/v1` or `apt/v1` | `held`, "held (`<versions>`, `<adapter>` has no rolling update path in this build)" | nothing |
| An executable of the tool's name in another owner's directory (for uv, also a file or a foreign link at `~/.local/bin/<name>`) | a blocker, `E_CONFLICT`: "declared for `<adapter>` but an executable `<path>` exists; a declared owner is not permission to replace another owner" | not approved |

A tool the provider does not list is planned as docs/packages.md plans it: `install`, with the same owner check. A probe that cannot run is `E_PROBE_FAILED`. A bad name or policy is `E_MANIFEST_INVALID` in both modes.

**Never run:** `mise prune`, `mise uninstall`, `mise upgrade`, `mise use`, `mise self-update`, `uv tool uninstall`, `uv self update`, and `uv tool upgrade` of a held tool. A test asserts that every provider call of a rolling update is a listing, `mise install` or `uv tool upgrade`. No version is removed and no configuration file is written. Every installed version, a project's `.mise.toml` or `.tool-versions`, and mise's selection for the current directory stay as they were. `uv` runs with `UV_PYTHON_DOWNLOADS=never`, so a tool whose interpreter is missing fails (`failed`, journaled) instead of installing a Python the plan does not show.

**Identities.** The plan row reports the declared identity (`name`, `update_policy`, the adapter) and the installed identity (every version the provider lists for the name, in the provider's order; a prefix such as `go@1.27` lists the versions that match). The selected identity is the provider's decision: mise selects per directory from its configuration, which the manager does not evaluate before approval, and uv keeps one version per tool.

**Interruption.** An interrupted upgrade follows the packages rule (docs/packages.md, "Interrupted install"). The installed versions, old and new, stay listed (test `an_interrupted_upgrade_keeps_the_installed_versions_and_blocks_until_resolved`).

## Upgrading mise or uv themselves

The manager does not upgrade either. Both come from Homebrew on the inventoried machines, and `homebrew/v1` has no rolling update path. The manager is a static binary that needs neither at run time (architecture section 2), so `state recover`, `state resolve` and `doctor` do not depend on a provider.

## Decisions where the contract was silent

| Question | Choice |
| --- | --- |
| Command shape | A `--update` flag on `packages`. `update` (DFM-31, not built) is to compose sources, locks and every kind. `sync` does not compose packages (docs/sync.md). |
| Candidate version | Not shown in the plan. Discovering it needs the provider's registry (`mise latest`, uv's index), which is the network, and a plan is read-only. The provider resolves it at apply time after approval. DESIGN.md section 7 item 1 asks for the eligible candidate before mutation. This gap stays open until `update` gets a network phase from trusted origins. |
| What `rolling` means for a pin | A declared `tool@version` is a prefix: `mise install tool@1.27` resolves the newest `1.27.x`. A declared version the listing contains as it is, is an exact pin, and the row holds it, because `mise install node@20.20.2` cannot advance. The rule reads the listing, not the number of components, because the listing format differs by backend. Advancing a pin is a manifest edit. |
| Policy values | Only `rolling` rolls. `hold` and `native-request` hold with the value in the row. A typo is refused, not held silently. |
| A copy from another owner | Under plain `packages`, a tool the declared provider lists is `current` even when another owner also has a copy. Under `--update` that copy is a blocker, because an upgrade would deepen the duplicate ownership. uv links its entry points into `~/.local/bin`, which other installers use too. There, only a link whose target is under `~/.local/share/uv/tools` is uv's. |
| Local builds | A mise version starting with `ref:`, `ref-`, `path:` or `path-` holds the tool. The check reads the whole listing before the declared prefix narrows it, so `go@1.27` is held by an installed `ref:feature`. The spellings `ref-` and `path-` are a guess at mise's listing, marked as a shortcut in the code. |
| `mise install` as the upgrade | Chosen over `mise upgrade`, which reads and can edit mise's configuration, and over `mise latest` plus `mise install`, which is two network calls. The selector is always explicit (`<tool>@latest` or `<tool>@<prefix>`), because `MISE_NO_CONFIG=1` hides the configuration a bare `mise install <tool>` reads. The look-alike in the test refuses a bare name. |
| `uv tool list` names | Compared as uv normalizes them (PEP 503: lowercase, each run of `_`, `.` and `-` as one `-`). The first word of a `<name> v<version>` line, so a source note after the version does not break the read. The receipt directory is looked up by the same name. |
| uv receipt forms | Only a receipt whose requirements are all tables, without a source key and without a version `specifier`, and with no `[tool.options]` entry, rolls. `uv tool upgrade` respects the recorded constraint, so a tool installed as `ruff==0.5` cannot advance and is held. A string requirement hides its source from the table keys, so any string requirement holds. uv persists index, find-links, prerelease, resolution and build settings in `[tool.options]`, and `uv tool upgrade` reuses them, so such an upgrade could fetch from an index the plan never showed. A harmless key (`compile-bytecode`) holds too, because an allow list of uv's options would need upkeep with every uv release. |
| Receipt | `command = packages` for both modes, with `update = true` for a rolling update. An update that changed nothing still writes a receipt, because the provider was asked (the journal holds its `intent` and `done`). |
| `homebrew/v1` under `--update` | Held with the reason in the row, not a blocker, so a module that mixes brew and mise tools can still run `packages --update`. A rolling Homebrew upgrade upgrades dependents and needs its own owner check. |

## Known limits

- **No candidate in the preview.** The reviewer approves "the provider's newest" without seeing the version.
- **Selected identity.** Which version a shim selects in a given directory is mise's decision. `doctor` does not probe tools.
- **mise backends.** A tool of a mise backend (`npm:`, `go:`, `github:`, `ubi:`, `vfox:` and the others) resolves its newest version from that backend's origin. The backend's origin, not a plan row, decides what is fetched, as for the install path.
- **`mise ls` without configuration.** The listing shows every installed version and no `active` or `requested_version` field. The adapter does not read those.
- **Evidence scope.** Tests use the `test-isolation` build and `mise`, `uv` and `brew` look-alikes. `mise ls --json`, `mise --version` and `uv tool list` were run by hand against the real tools once to learn their output. No test runs a real install or upgrade.
