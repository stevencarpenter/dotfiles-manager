# Packages (DFM-23)

Task: DFM-23, the `homebrew/v1`, `mise/v1` and `apt/v1` package adapters and the `packages` command. This document records what the first package provisioning does and where it resolves or differs from [docs/architecture.md](architecture.md) (the contract) and DESIGN.md section 8. The contract wins where this document is silent. The planner is in [docs/planner.md](planner.md), apply in [docs/apply.md](apply.md), the secrets scope in [docs/secrets.md](secrets.md).

## What exists

| Part | Location |
| --- | --- |
| `Scope::Packages`, `PackageState`, `Action::Install` and the package decision (`decide_package`) | `crates/model/src/plan/` |
| Probes (`brew list --versions`, `mise ls --installed --json`, `dpkg-query -W`), the other-owner check, installs (`brew install`, `mise install`) | `crates/cli/src/adapters/package.rs` |
| `Generator::installs`, `Generator::probe`, `Generator::install` | `crates/engine/src/generate.rs` |
| Execution (`try_install`: re-probe, intent, install, re-probe, outcome) | `crates/engine/src/apply.rs` |
| `packages` command (the `apply` command over the packages scope) | `crates/cli/src/apply_cmd.rs`, `crates/cli/src/lib.rs` |
| Code `E_PROBE_FAILED` (3) | `crates/model/src/refusal.rs` |
| Tests with `brew` and `mise` look-alikes | `crates/cli/tests/packages.rs` |

The child-process runner is the one `secrets` uses (docs/secrets.md, "How `op` is run"): fixed search path, checked executable, rebuilt environment, own process group, timeout, SIGTERM then SIGKILL on a stop request.

## The command

```
dotfiles-manager packages --root <dir> [--preview] [--yes-plan <digest>]
```

`packages` is `apply` over the packages scope: every `package` resource is planned, every other resource is `not_in_scope` ("link is configuration, outside a packages plan"), and nothing is removed. Planning a package needs the provider's installed-state listing, so **`packages` probes the providers before it plans**, read-only, and hands the answers to the plan builder (nothing in `plan_cmd.rs` starts a process; a gate test keeps it so). The manager's signal handlers are installed before the first probe, so a stop request ends the probe instead of leaving it running, starts no further provider, and exits 130 or 143 before anything is written. `plan`, `apply` and `doctor` start no package provider and list a package as `not_in_scope` (or, for `doctor`, `D_PROBING_NOT_BUILT`); `secrets` starts only `op`.

Order of one run: resolve, probe, plan and print; the gates that need no write; without `--yes-plan` exit 3 (`E_APPROVAL_REQUIRED`); the lock, the second plan (the probes run again), the handlers; one operation per package; receipt (`command = packages`) and report. `--allow-disruptive` and `--allow-adopt` are usage errors: a packages plan removes and adopts nothing.

## The adapters

Value: `name` (the package as the provider names it: letters, digits, `.`, `_`, `-`, `+`, `@`, `/`, `:`, not starting with `-`, at most 128 characters; a bad name is `E_MANIFEST_INVALID`), `update_policy` (a string, kept as data) and `downgrade` (optional, kept as data). A tap-qualified Homebrew name (`homebrew/core/atuin`) is probed by its short name, which is how `brew list` prints it, and installed in full. A mise `tool@version` is listed by its tool name and counts as installed when an installed version is the declared one or extends it by whole components (`node@20` matches `20.20.2` and not `2.1.0`); `mise install tool@version` installs it. A selector only mise can resolve (`latest`, `lts`, `lts-*`, `sub-*`, `ref:`, `path:`, `prefix:`, `system`) is `E_MANIFEST_INVALID`: the probe runs without the network and could not tell whether such a version is installed. Nothing else reads a version: a package plan installs what is missing and never upgrades or downgrades (DESIGN.md section 8: config application does not refresh rolling versions; `update` is DFM-31).

| Adapter | Probe (read-only) | Installed when | Install |
| --- | --- | --- | --- |
| `homebrew/v1` | `brew list --versions <name>` with `HOMEBREW_NO_AUTO_UPDATE=1`, `HOMEBREW_NO_ANALYTICS=1`, `HOMEBREW_NO_ENV_HINTS=1`, `HOMEBREW_NO_INSTALL_CLEANUP=1`, `HOMEBREW_NO_INSTALL_UPGRADE=1`, `HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK=1` (an install upgrades neither the formula nor its installed dependents; these reach only the manager's `brew` child); 60 s | exit 0 and a line `<name> <version>...` | `brew install <name>`, same variables, 30 min |
| `mise/v1` | `mise ls --installed --json <tool>` with `MISE_YES=1`, `MISE_NOT_FOUND_AUTO_INSTALL=false` and `MISE_NO_CONFIG=1` (no config file is loaded before approval: config evaluation can run hooks, env sources and remote includes; JSON because the table truncates a long tool name); 60 s | exit 0 and a JSON array with an installed `version` (the declared version or a longer one by whole components, if any); `[]` is missing; any other answer is `E_PROBE_FAILED` | `mise install <name>` with the same variables, `MISE_NO_CONFIG=1` included, so no config hook (`postinstall`) or setting the plan does not show applies: a bare tool installs mise's newest version, a declared `tool@version` that version; 30 min |
| `apt/v1` | `dpkg-query -W -f=${Status} <name>`; 60 s | exit 0 and `install ok installed` | None. Installing needs root, and a HOME adapter never escalates (architecture section 8, privilege). A missing apt package is a blocker, `E_PRIVILEGE_UNSUPPORTED`, with the command to run by hand. |

A probe that cannot run (program not found or failed its checks), times out, or answers in a shape the adapter does not know is `Unknown` with a reason: the plan has a blocker, `E_PROBE_FAILED`, exit 3 (architecture section 8 item 9: unknown support fails closed). The same code is used when the probe under the lock cannot answer. Provider standard error is bounded and never printed or persisted.

**One owner (DESIGN.md section 8).** When the declared provider does not list the package, the adapter looks, with `stat` calls only (the entry, then what a link points to), for an executable named like the package's short name (`atuin` for `homebrew/core/atuin`, `node` for `node@20`) in the other prefixes a package can come from: the other search-path directories, `~/.nix-profile/bin`, `/nix/var/nix/profiles/default/bin`, `~/.local/bin`, `~/.cargo/bin` and the mise shim directory `~/.local/share/mise/shims`. The declared provider's own directories are skipped by path component (`homebrew`) or exact path (the mise shims), never by substring. A hit is `E_CONFLICT`: "installed by another owner; a declared owner is not permission to replace it". A directory that cannot be examined makes the state `Unknown` (`E_PROBE_FAILED`), so the check fails closed. The check compares the package name with the executable name, so a package whose binary has another name (`ripgrep` installs `rg`) is not caught; the provider's own listing is still authoritative for what it installed.

## One operation

1. **Plan time.** The probe runs. `Installed(version)`: `current`, "name is installed by homebrew/v1 (version)". `Missing(deps)`: `install`, planned, with the precondition `probe:<adapter>:<name> = missing` (or `missing:with=<dep>,...` when the install adds dependencies; the effect names them, so the approved digest covers them). `Upgrades(deps)`: a blocker, `E_CONFLICT`, because the install would upgrade installed dependencies and a package plan upgrades nothing. `SourceBuild(names)`: a blocker, `E_PLATFORM_UNSUPPORTED`, because the package or a missing dependency has no prebuilt binary for this machine and a source build adds build-time dependencies the listing does not name. `OtherOwner(why)`, `Unsupported(why)`, `Unknown(why)`: a blocker with fix text. The operation id is `<resource>@package`; it has no path, no mode and no ledger entry.
2. **Apply.** The probe runs again under the lock: `Installed` now is `current` (someone installed it meanwhile; the state matches); `Missing` with the planned dependencies or a subset of them goes on (an earlier install in the same run can add a dependency two packages share); anything else blocks with `E_PLAN_STALE`. Then the stop check, the `intent` entry (no recovery output), the install, a third probe as the postcondition (`Installed`, else `failed`), the `done` or `failed` entry. No ledger entry is written: a package is not an owned file, and removal is not planned (DESIGN.md section 8: no uninstall by inference).
3. **Receipt.** Class `normal`, status `updated`, `current` or `failed`, no fingerprints, no owned outputs, no reload.

## Decisions where the documents were silent

| Question | Choice |
| --- | --- |
| Command shape | A separate `packages` command. DESIGN.md names `sync` as the command that composes provisioning; `sync` is DFM-30, and `packages` is the package part of it. |
| Versions and pins | Not read and not written. The value schema has no version field and the source lock has no package pin yet (DFM-31). Install means "make present". |
| Interrupted install | The intent has no path, so recovery cannot probe it: the resource blocks (`E_OP_INTERRUPTED`) until `state resolve <journal-id> --as failed`, then the next `packages` run probes and installs again. |
| Probes at plan time | The `packages` command runs them before it plans; the read-only commands do not. Architecture section 8 names "a package installed-version listing" as a candidate probe to verify in implementation; the two listings here are verified by their fixture tests against look-alikes, and `brew list --versions` (plain and tap-qualified) and `mise ls --installed` were run by hand against the real tools once. |
| `apt/v1` | Probe only. No privileged adapter exists (architecture section 15: no Linux provider in this contract). The binary runs only on macOS, where the probe program does not exist, so an apt resource in a plan is `E_PROBE_FAILED`. |
| Owner check | By executable name only (above). |
| Homebrew dependencies | `brew install` installs a formula's missing dependencies and upgrades outdated installed ones. The probe asks, from local data only (auto-update off, no download): `brew deps <name>`, then `brew list --versions <deps>`, `brew info --json=v2 <name> <missing deps>` and `brew outdated --json=v2 <installed deps>`. Missing dependencies are listed in the plan and installed with the package. A package or missing dependency with no bottle for this machine blocks the plan (`E_PLATFORM_UNSUPPORTED`): Homebrew reports only this machine's bottle tags (its API cache is per platform), so an empty bottle file list means a source build, whose build-time and implicit dependencies `brew deps` does not list. An outdated installed dependency blocks the plan (`E_CONFLICT`, fix: upgrade it yourself). `brew install --dry-run` was not used because it downloads manifests. These calls load the formula's Ruby file, which for a non-official tap is third-party code, so they run only for an official name: one that `brew formulae` or `brew casks` prints bare (official) rather than qualified (tap), or `homebrew/core/<name>` and `homebrew/cask/<name>`. Any other package that is missing blocks the plan (`E_SOURCE_UNTRUSTED`, fix: install it by hand). Every later call, the install included, names the package in the namespace that matched (`homebrew/core/<name>` for a formula, `--cask <name>` for a cask) and its dependencies as `homebrew/core/<dep>`, so brew cannot resolve the same short name to a tap's formula. The install checks the name again before it runs. The mise adapter has no dependency listing, so its `Missing` lists none. |

## Known limits

- **Rerun cost.** Every `packages` run probes every package twice (plan and locked re-plan); a missing package is probed twice more (before and after its install). Homebrew's listing takes about a second, and a missing formula adds four local brew calls for its dependencies.
- **Dependency freshness.** `brew outdated` compares with Homebrew's local formula data. With auto-update off, a dependency is outdated only relative to the last `brew update` the owner ran.
- **Not an update.** A rolling package that is installed stays at its installed version; `update` (DFM-31) is not built.
- **Signals.** A stop request during an install ends the provider (SIGTERM, 10 s, SIGKILL). A half-installed package is the provider's state, not the manager's; the operation is `failed` and the next run probes again.
- **Evidence scope.** Tests use the `test-isolation` build and `brew` and `mise` look-alikes. No real install was run by a test.
