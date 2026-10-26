# Packages

The `homebrew/v1`, `mise/v1`, `uv-tool/v1` and `apt/v1` package adapters and the `packages` command. The rolling update (`packages --update`) is in [docs/toolchains.md](toolchains.md). This document describes the behavior at HEAD.

| Part | Location |
| --- | --- |
| `Scope::Packages`, `PackageState`, `Action::Install` and the package decision (`Planner::package`) | `crates/model/src/plan/` |
| Probes, the other-owner check and installs | `crates/cli/src/adapters/package.rs` |
| `Generator::installs`, `Generator::probe`, `Generator::install` | `crates/engine/src/generate.rs` |
| Execution (`try_install`: re-probe, intent, install, re-probe, outcome) | `crates/engine/src/apply.rs` |
| `packages` command (`apply` over the packages scope) | `crates/cli/src/apply_cmd.rs`, `crates/cli/src/lib.rs` |
| Tests with `brew`, `mise` and `dpkg-query` look-alikes | `crates/cli/tests/packages.rs` |

Providers run through the bounded runner (docs/secrets.md, "How `op` is run").

## The command

```
dfm packages [--root <dir>] [--preview] [-y|--yes-plan <digest>] [--update]
```

`packages` is `apply` over the packages scope. Every `package` resource is planned. Every other resource is `not_in_scope` ("link is configuration, outside a packages plan"). Nothing is removed, upgraded or downgraded. `--root` defaults to the root the binding records. `--allow-disruptive` and `--allow-adopt` are usage errors (exit 2), because a packages plan removes and adopts nothing.

Planning needs each provider's installed-state listing, so `packages` probes the providers read-only before it plans. The plan builder gets the answers. Nothing in `plan_cmd.rs` starts a process, and a gate test keeps it so. `plan`, `apply`, `secrets` and `doctor` start no package provider. `plan` and `apply` list a package as `not_in_scope`, and `doctor` lists it as `D_PROBING_NOT_BUILT`.

Order of one run:

1. The signal handlers go in.
2. Resolve, probe, plan and print.
3. The gates that need no write.
4. Without `--yes-plan`: exit 3 (`E_APPROVAL_REQUIRED`). A plan where every package is `current` and the journal has no open entry needs no digest: it prints `nothing to change: no approval needed` and exits 0 (docs/apply.md, step 4).
5. The writer lock and the second plan. The probes run again.
6. One operation per package, then the receipt (`command = packages`) and the report.

## The adapters

A package value has three fields:

- `name`: the package as the provider names it. Letters, digits, `.`, `_`, `-`, `+`, `@`, `/`, `:`, not starting with `-`, at most 128 characters. A bad name is `E_MANIFEST_INVALID`.
- `update_policy`: `rolling`, `hold` or `native-request`. Another value is `E_MANIFEST_INVALID`. Only `packages --update` acts on it (docs/toolchains.md).
- `downgrade`: optional, kept as data.

Names:

- A tap-qualified Homebrew name (`homebrew/core/atuin`) is probed by its short name, the rack Homebrew keeps it under. Its tap is checked in the install receipt. Only an official package is installed (see "Homebrew dependencies and trust").
- A mise `tool@version` is listed by its tool name. It counts as installed when an installed version is the declared one or extends it by whole components: `node@20` matches `20.20.2` and not `2.1.0`.
- A mise selector only mise can resolve (`latest`, `lts`, `lts-*`, `lts/*`, `sub-*`, `ref:`, `path:`, `prefix:`, `system`) is `E_MANIFEST_INVALID`. The probe runs without the network and cannot tell whether such a version is installed.
- A `uv-tool/v1` name is a package-index name. A Git, path or URL source, or a version, is `E_MANIFEST_INVALID` (docs/toolchains.md).

Nothing else reads a version. A package plan installs what is missing and never upgrades or downgrades.

| Adapter | Probe (read-only, 60 s) | Installed when | Install (30 min) |
| --- | --- | --- | --- |
| `homebrew/v1` | `brew --cellar` and `brew --caskroom`, then the version directories under `<Cellar or Caskroom>/<short name>`. No formula or cask file is loaded, so a tap's code never runs before approval, and a cask is found as a cask. | A version directory under the rack (`.metadata` and other dot entries skipped). For a tap-qualified name, every install receipt (`<keg>/INSTALL_RECEIPT.json`, or the cask's `.metadata/INSTALL_RECEIPT.json`) must name that tap in `source.tap`, else another owner (`E_CONFLICT`). An unreadable receipt is `E_PROBE_FAILED`. A bare name accepts any tap. | `brew install homebrew/core/<name>` for a formula, `brew install --cask <name>` for a cask. Official packages only. |
| `mise/v1` | `mise ls --installed --json <tool>`. JSON, because the table truncates a long tool name. | Exit 0 and a JSON array with an installed `version` that matches. `[]` is missing. Any other answer is `E_PROBE_FAILED`. | `mise install <tool>@latest` for a bare name, `mise install <tool>@<version>` for a declared `tool@version`. The selector is always sent, because `mise install <tool>` installs only the version mise's configuration requests, and that configuration is hidden. |
| `uv-tool/v1` | `uv tool list` | A `<name> v<version>` line for the name, compared as uv normalizes names. `No tools installed` or no line is missing. | `uv tool install <name>` |
| `apt/v1` | `dpkg-query -W -f=${Status} <name>` | Exit 0 and `install ok installed` | None. Installing needs root, and a HOME adapter never escalates. A missing apt package is a blocker, `E_PRIVILEGE_UNSUPPORTED`, with the command to run by hand. |

Provider environment (the variables reach only the manager's child):

- `brew`: `HOMEBREW_NO_AUTO_UPDATE=1`, `HOMEBREW_NO_ANALYTICS=1`, `HOMEBREW_NO_ENV_HINTS=1`, `HOMEBREW_NO_INSTALL_CLEANUP=1`, `HOMEBREW_NO_INSTALL_UPGRADE=1`, `HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK=1`. An install upgrades neither the formula nor its installed dependents.
- `mise`: `MISE_YES=1`, `MISE_NOT_FOUND_AUTO_INSTALL=false`, `MISE_NO_CONFIG=1`. No config file is loaded, because config evaluation can run hooks, env sources and remote includes. The install uses the same variables, so no config hook (`postinstall`) runs.
- `uv`: `UV_NO_CONFIG=1`, `UV_NO_PROGRESS=1`, `UV_PYTHON_DOWNLOADS=never`, and `UV_OFFLINE=1` for the probe only. No interpreter is downloaded.

A probe that cannot run (program not found or failed its checks), times out, or answers in a shape the adapter does not know is `Unknown` with a reason. The plan gets a blocker, `E_PROBE_FAILED`, exit 3: unknown support fails closed. The same code applies when the probe under the lock cannot answer.

### One owner

When the declared provider does not list the package, the adapter looks for an executable named like the package's short name (`atuin` for `homebrew/core/atuin`, `node` for `node@20`). It uses `stat` calls only (the entry, then what a link points to). It looks in the other prefixes a package can come from:

- the other search-path directories, less `/usr/bin`, `/bin`, `/usr/sbin` and `/sbin` (macOS system tools, which no package owns or replaces);
- `~/.nix-profile/bin` and `/nix/var/nix/profiles/default/bin`;
- `~/.local/bin` and `~/.cargo/bin`;
- the mise shim directory `~/.local/share/mise/shims`.

The declared provider's own directories are skipped by path component (`homebrew`) or exact path (the mise shims), never by substring. For uv, only a link at `~/.local/bin/<name>` into `~/.local/share/uv/tools` is uv's own. A hit is `E_CONFLICT`: "installed by another owner; a declared owner is not permission to replace it". A directory that cannot be examined makes the state `Unknown` (`E_PROBE_FAILED`), so the check fails closed.

The check compares the package name with the executable name. A package whose binary has another name (`ripgrep` installs `rg`) is not caught. The provider's own listing is still authoritative for what it installed.

### Homebrew dependencies and trust

`brew install` installs a formula's missing dependencies and upgrades outdated installed ones. The probe asks brew from local data only (auto-update off, no download):

1. `brew deps <name>`, then the Cellar racks of those dependencies.
2. One `brew ruby` call that asks whether brew would pour a bottle for the package and each missing dependency (`Formula#bottle && pour_bottle?`). The bottle may be one for an older macOS, and a pour condition such as `clt_installed` counts when it holds.
3. `brew outdated --json=v2 <installed deps>`.

Results:

- Missing dependencies are listed in the plan and installed with the package.
- A package or missing dependency that brew would not pour as a bottle blocks the plan (`E_PLATFORM_UNSUPPORTED`). A source build adds build-time and implicit dependencies that `brew deps` does not list.
- An outdated installed dependency blocks the plan (`E_CONFLICT`, fix: upgrade it yourself).

These calls load the formula's Ruby file, which for a non-official tap is third-party code. So they run only for an official name: one that `brew formulae` or `brew casks` prints bare, or `homebrew/core/<name>` and `homebrew/cask/<name>`. Any other missing package blocks the plan (`E_SOURCE_UNTRUSTED`, fix: install it by hand). Every later call, the install included, names the package in the namespace that matched (`homebrew/core/<name>` or `--cask <name>`) and its dependencies as `homebrew/core/<dep>`. Brew therefore cannot resolve the same short name to a tap's formula. The install checks the name again before it runs. `brew install --dry-run` is not used, because it downloads manifests. The mise and uv adapters have no dependency listing.

## One operation

1. **Plan time.** The probe runs. The operation id is `<resource>@package`. It has no path, no mode and no ledger entry.
   - `Installed(version)`: `current`, "name is installed by homebrew/v1 (version)".
   - `Missing(deps)`: `install`, planned, with the precondition `probe:<adapter>:<name> = missing`, or `missing:with=<dep>,...` when the install adds dependencies. The effect names them, so the approved digest covers them.
   - `Upgrades(deps)`: a blocker, `E_CONFLICT`, because the install would upgrade installed dependencies.
   - `SourceBuild(names)`: a blocker, `E_PLATFORM_UNSUPPORTED`.
   - `Unofficial(why)`: a blocker, `E_SOURCE_UNTRUSTED`.
   - `OtherOwner(why)`: a blocker, `E_CONFLICT`. `Unsupported(why)` (apt): `E_PRIVILEGE_UNSUPPORTED`. `Unknown(why)`: `E_PROBE_FAILED`. Each has fix text.
2. **Apply.** The probe runs again under the lock. `Installed` is now `current`. `Missing` with the planned dependencies or a subset of them goes on, because an earlier install in the same run can add a shared dependency. Anything else blocks with `E_PLAN_STALE`. Then the stop check, the `intent` entry (no recovery output), the install, a third probe as the postcondition (`Installed`, else `failed`), and the `done` or `failed` entry. No ledger entry is written: a package is not an owned file, and removal is not planned (DESIGN.md section 8: no uninstall by inference).
3. **Receipt.** Class `normal`, status `updated`, `current` or `failed`, no fingerprints, no owned outputs, no reload.

**Interrupted install.** The intent has no path, so recovery cannot probe it. The resource blocks with `E_OP_INTERRUPTED` until `state resolve <journal-id> --as failed`, even when the provider lists it as installed. The next `packages` run probes and installs only if the package is missing. The same rule applies to a rolling upgrade (docs/toolchains.md), a trial install (docs/trial.md) and a service action (docs/lifecycle.md).

## Decisions

| Question | Choice |
| --- | --- |
| Command shape | A separate `packages` command. `sync` does not compose it (docs/sync.md): run `packages` after a sync when the plan lists packages. |
| Versions and pins | Not read and not written. The value schema has no version field and the source lock has no package pin. Install means "make present". |
| Probes at plan time | `packages` runs them before it plans. The read-only commands do not. Fixture tests against look-alikes verify the listings. `brew list --versions` (plain and tap-qualified) and `mise ls --installed` were run by hand against the real tools once. |
| `apt/v1` | Probe only. No privileged adapter exists. The binary runs only on macOS, where `dpkg-query` does not exist, so an apt resource in a plan is `E_PROBE_FAILED`. |
| Owner check | By executable name only (see "One owner"). |

## Known limits

- **Rerun cost.** Every `packages` run probes every package twice (plan and locked re-plan). A missing package is probed twice more (before and after its install). Homebrew's listing takes about a second. A missing formula adds local brew calls for the official check and its dependencies (`formulae`, `casks`, `deps`, `--cellar`, `--caskroom`, `ruby`, `outdated`).
- **Dependency freshness.** `brew outdated` compares with Homebrew's local formula data. With auto-update off, a dependency is outdated only relative to the last `brew update` the owner ran.
- **No update.** An installed package stays at its installed version. `packages --update` (docs/toolchains.md) upgrades rolling mise and uv tools only. `update` (DFM-31) is not built.
- **Signals.** A stop request during an install ends the provider. A half-installed package is the provider's state. The operation is `failed`, and the next run probes again. A stop that cuts off the postcondition probe is reported as the stop (exit 130 or 143), and the journal says the install may have completed. A stop seen right after the pre-install probe starts nothing.
- **Evidence scope.** Tests use the `test-isolation` build and `brew`, `mise` and `dpkg-query` look-alikes (`crates/cli/tests/packages.rs`); the `uv` look-alike is in `crates/cli/tests/toolchains.rs`. No test runs a real install.
