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
| `brew` | `brew update`; `brew outdated --json=v2`; `brew upgrade <names>`; `brew outdated --json=v2` | each outdated name's installed and newest version | `brew` on the provider path (`/opt/homebrew/bin`) |
| `claude` | `claude update` | `claude --version` | `~/.local/bin/claude` |
| `codex` | `codex update` | `codex --version` | `~/.codex/packages/standalone/current/bin/codex` |
| `opencode` | `opencode upgrade` | `opencode --version` | `~/.opencode/bin/opencode` |
| `skills` | `mise exec -- npx skills update` | none | `mise` on the provider path |

- **brew** upgrades every outdated formula and cask, not only declared ones. Version-sensitive tools are pinned with mise, per project or globally. `brew upgrade` gets the outdated names, so each one is accounted for, and an active trial's package can be left out. An empty list is `current` and runs no upgrade. Homebrew runs with its defaults (the dependents check and cleanup stay on). The manager sets `HOMEBREW_NO_ANALYTICS=1` and `HOMEBREW_NO_ENV_HINTS=1`, and `HOMEBREW_NO_AUTO_UPDATE=1` for every command after `brew update`.
- **A native tool** (`claude`, `codex`, `opencode`) must exist at its owner path. If it does not, it is `blocked` with that path, and no other copy (for example the Homebrew cask) is used. The runner's executable checks apply: a link that leaves the owner's prefix is `blocked`.
- **A second copy.** For each native tool, `brew list --versions <name>` and `brew list --cask --versions <name>` look for a Homebrew copy. A copy found is a note on the row (`second copy: Homebrew cask codex 0.164.0`). It is never removed or chosen.
- **skills** reports no version. Exit 0 is `current` with the note `skills update ran; it reports no version`.

## Statuses

| Status | When |
| --- | --- |
| `updated` | the version changed (brew: every upgraded name left the outdated list) |
| `current` | exit 0 and the same version (brew: nothing outdated) |
| `held` | brew: every outdated name belongs to an active trial |
| `pending` | a stop request (SIGINT, SIGTERM) ended or prevented it, or the update ran and the new version could not be read |
| `failed` | a command exited nonzero, timed out or was ended by a signal; brew: a name stayed outdated or the list was not valid JSON |
| `blocked` | the tool is missing from its owner path, fails the executable checks, or its version cannot be read before the update |

A failure note gives the command, its exit code and the size of its standard error. The text is withheld (docs/secrets.md); run the command by hand to see it. A partial `brew upgrade` is `failed`, and the notes list each name as upgraded or still outdated.

## In sync

1. The four scopes run as docs/sync.md describes.
2. A refused run (an error exit) runs no updater. A skipped or blocked scope does not stop them.
3. `--no-upgrade` prints `upgrade: skipped (--no-upgrade)` and runs none.
4. The step also runs after "nothing to change".
5. Under the writer lock, each updater runs and prints one row: `  <status> <name> <before> -> <after>`, then its notes.
6. One receipt is written (`command = "upgrade"`, no plan digest, no approval), with one operation per updater (`resource = "updater:<name>"`, `adapter = "updater/v1"`). An updater that is not `updated`, `current` or `held` makes the receipt `failed`, exit 1, while sync itself exits 0.
7. The `scopes:` block ends with `  upgrade: <status>=<count>...`, and each `failed`, `blocked` or `pending` updater adds a `warning:` line at the end of standard output.
8. A stop request during the step ends that updater, reports the rest `pending`, writes the receipt and ends the run with the stop's exit code. No journal entry is written, so the next sync runs every updater again.

`--preview` lists each updater with what it would run and its resolved path, and runs nothing.

Updaters run detached: standard input is closed and no terminal is attached. A step that needs a password or a prompt, such as a cask whose installer runs `sudo`, fails. The `brew` row names the package that stayed outdated. Upgrade it by hand.

Timeouts: 30 minutes for `brew upgrade`, 10 minutes for every other command.

## Known limits

- **No candidate version in the preview.** The newest version is what each provider resolves at run time.
- **`sudo` casks fail** under the detached run.
- **skills reports no version**, so it is never `updated`.
- **Second copies** are looked for in Homebrew only, not in npm or mise.
- **Evidence scope.** Tests use look-alike updaters. No test runs a real updater.
