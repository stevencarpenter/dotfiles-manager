# Upgrade

The upgrade step of `dfm sync` (DFM-31, first slice): after the four scopes, each updater the profile declares runs through its own owner's updater and ends in one status. This document describes the behavior at HEAD.

| Part | Location |
| --- | --- |
| `Updater`, the fixed set | `crates/model/src/updater.rs` |
| The profile key `updaters` | `crates/model/src/resolve/docs.rs` (docs/resolver.md, rule 16) |
| One updater: `run`, `preview`, `Row` | `crates/cli/src/adapters/updater.rs` |
| The step in sync: `upgrade`, `upgrade_preview` | `crates/cli/src/sync_cmd.rs` |
| Doctor's `D_UPGRADE` row | `crates/cli/src/doctor_cmd.rs` |
| Tests with look-alike updaters | `crates/cli/src/adapters/updater.rs` (unit), `crates/cli/tests/sync.rs` (the upgrade section) |

## Declaring the updaters

```toml
# profiles/personal-mac.toml
updaters = ["claude", "codex", "opencode", "brew", "skills"]
```

The names come from a fixed set built into the manager. A document cannot add a command, so a commit pulled from GitHub cannot make sync run a program the manager does not know. The updaters run in the declared order, one at a time. An absent or empty list runs none.

A manager built before this key refuses a profile that has it (`E_FIELD_UNKNOWN`). Declare `updaters` only after every machine that uses the profile runs a manager that knows it.

## The fixed set

| Name | Commands | Version before and after | Owner path |
| --- | --- | --- | --- |
| `brew` | `brew update`; `brew outdated --json=v2`; `brew upgrade --formula <names>`; `brew upgrade --cask <names>`; `brew outdated --json=v2` | each outdated name's installed and newest version | `brew` on the provider path (`/opt/homebrew/bin`) |
| `claude` | `claude update` | `claude --version` | `~/.local/bin/claude` |
| `codex` | `codex update` | `codex --version` | `~/.codex/packages/standalone/current/bin/codex` |
| `opencode` | `opencode upgrade` | `opencode --version` | `~/.opencode/bin/opencode` |
| `skills` | `mise exec -- npx skills update` | none | `mise` on the provider path |

- **brew** upgrades every outdated formula and cask, not only declared ones. A name that `brew outdated --json=v2` marks `"pinned": true` (`brew pin`) is left out of the upgrade and its check, with the note `formula node held (brew pin)`. Version-sensitive tools are pinned with mise, per project or globally. `brew upgrade` gets the outdated names, one call per kind, so each formula and each cask is accounted for as itself, also when a formula and a cask share a name. Notes read `formula uv 0.13.5 -> 0.13.6 upgraded` or `cask codex 0.164.0 still outdated (0.165.0)`. An empty list is `current` and runs no upgrade. Homebrew runs with its defaults (the dependents check and cleanup stay on). The manager sets `HOMEBREW_NO_ANALYTICS=1` and `HOMEBREW_NO_ENV_HINTS=1`, and `HOMEBREW_NO_AUTO_UPDATE=1` for every command after `brew update`.
- **A native tool** (`claude`, `codex`, `opencode`) must exist at its owner path. If it does not, it is `blocked` with that path, and no other copy (for example the Homebrew cask) is used. The runner's executable checks apply: a link that leaves the owner's prefix is `blocked`.
- **A second copy.** For each native tool, `brew list --versions` and `brew list --cask --versions` look for a Homebrew copy under Homebrew's name for it: the cask `claude-code` (the `claude` cask is the desktop app), the formula and cask `codex`, the formula `opencode`. The check runs after every updater ran, so the version is the one `brew` left, and each copy found is a line after the rows (`  note: codex second copy: Homebrew cask codex 0.165.0`). It is never removed or chosen.
- **skills** reports no version. Exit 0 is `current` with the note `skills update ran; it reports no version`.

## Statuses

| Status | When |
| --- | --- |
| `updated` | the version changed (brew: every upgraded name left the outdated list) |
| `current` | exit 0 and the same version (brew: nothing outdated) |
| `pending` | a stop request (SIGINT, SIGTERM) ended or prevented it, or the update ran and the new version could not be read |
| `failed` | a command exited nonzero, timed out or was ended by a signal; brew: a name stayed outdated or the list was not valid JSON |
| `blocked` | the tool, or `mise` for skills, is missing from its owner path, fails the executable checks, or its version cannot be read (or is empty) before the update; brew: `brew` cannot be resolved on the provider path |

A failure note gives the command, its exit code and the size of its standard error. The text is withheld (docs/secrets.md); run the command by hand to see it. A partial `brew upgrade` is `failed`, and the notes list each name as upgraded or still outdated.

## Trials

A trial does not hold `brew`. The owner's decision of 2026-10-31 replaces DFM-31.C09: a tool is trialed without an effect on any other update, and it is updated with the other packages as if it were already adopted. `brew` runs as if no trial existed and upgrades a trial's formula or cask with every other outdated package. No updater reads the trial records, so an unreadable record or an interrupted trial write does not change the run. A missing trials directory still refuses the whole run before any scope, as state loss (`E_STORAGE_INVALID`): the store refuses every writer on a missing state directory (docs/state.md).

## In sync

1. After the plan, before publication and the scopes, each updater is listed with what it would run and its resolved path, or `blocked: <why>` (DFM-31.C02). Nothing runs yet.
2. The four scopes run as docs/sync.md describes.
3. A refused run (an error exit) runs no updater. A skipped or blocked scope does not stop them.
4. `--no-upgrade` prints `upgrade: skipped (--no-upgrade)` and runs none.
5. The step also runs after "nothing to change".
6. Under the writer lock, the profile is resolved again from `--root`, because another sync may have changed it since the review. An updater runs only when the review listed it and the profile still declares it; one added meanwhile runs in the next sync, after its review. A profile that declared none in the review takes no lock. Each updater that runs prints one row: `  <status> <name> <before> -> <after>`, then its notes. After the last row, the second copies of the native tools are read (see "The fixed set").
7. One receipt is written (`command = "upgrade"`, no plan digest, no approval), with one operation per updater (`resource = "updater:<name>"`, `adapter = "updater/v1"`). An updater that is not `updated` or `current` makes the receipt `failed`, exit 1, while sync itself exits 0.
8. The `scopes:` block ends with `  upgrade: <status>=<count>...`, and each `failed`, `blocked` or `pending` updater adds a `warning:` line at the end of standard output.
9. A stop request during the step ends that updater, reports the rest `pending`, writes the receipt and ends the run with the stop's exit code (130 or 143). The warnings still print. No journal entry is written, so the next sync runs every updater again.
10. An error of the step itself (the writer lock is held by another command, the state cannot be read, the receipt cannot be written) keeps the scope summary, reports `upgrade: failed (<code>)` and exits with that error.

`--preview` lists each updater with what it would run and its resolved path, and runs nothing.

Updaters run detached: each starts in a new session (`setsid`, `Runner::detach`), with no controlling terminal and standard input closed. A step that needs a password or a prompt, such as a cask whose installer runs `sudo`, fails at once instead of waiting at the terminal. The `brew` row names the package that stayed outdated. Upgrade it by hand.

Timeouts: 30 minutes for `brew upgrade`, 10 minutes for every other command.

## Known limits

- **No candidate version in the preview.** The newest version is what each provider resolves at run time.
- **`sudo` casks fail** under the detached run.
- **skills reports no version**, so it is never `updated`.
- **Second copies** are looked for in Homebrew only, not in npm or mise.
- **Evidence scope.** Tests use look-alike updaters. No test runs a real updater.
