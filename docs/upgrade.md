# Upgrade

The upgrade step of `dfm sync` (DFM-31, first slice): after the four scopes, each updater the profile declares runs through its own owner's updater and ends in one status. This document describes the behavior at HEAD.

| Part | Location |
| --- | --- |
| `Updater`, the fixed set | `crates/model/src/updater.rs` |
| The profile key `updaters` | `crates/model/src/resolve/docs.rs` (docs/resolver.md, rule 16) |
| One updater: `run`, `preview`, `Row`, the source check `trust_notes` | `crates/cli/src/adapters/updater.rs` |
| Source identities and grants | `crates/model/src/lock.rs` (`source_identity`, `Grants`) |
| The step in sync: `upgrade`, `upgrade_preview` | `crates/cli/src/sync_cmd.rs` |
| Doctor's `D_UPGRADE` row | `crates/cli/src/doctor_cmd.rs` |
| Tests with look-alike updaters | `crates/cli/src/adapters/updater.rs` (unit), `crates/cli/tests/sync.rs` (the upgrade section) |

## Declaring the updaters

```toml
# profiles/personal-mac.toml
updaters = ["claude", "codex", "opencode", "brew", "skills", "agents"]
```

The names come from a fixed set built into the manager. A document cannot add a command, so a commit pulled from GitHub cannot make sync run a program the manager does not know. The updaters run in the declared order, one at a time. An absent or empty list runs none.

A manager built before this key refuses a profile that has it (`E_FIELD_UNKNOWN`). Declare `updaters` only after every machine that uses the profile runs a manager that knows it.

## The fixed set

| Name | Commands | Version before and after | Owner path |
| --- | --- | --- | --- |
| `agents` | working copy: `git status --porcelain`; managed copy: `git status --porcelain`, `git rev-parse --abbrev-ref @{u}`, `git fetch`, `git rev-list --count @{u}..HEAD`, `git pull --ff-only`; then `scripts/install-agent-registry.sh <copy>` | `git rev-parse --short HEAD` of the copy | the working copy `~/projects/agents` or the managed copy `~/.local/share/agent-registry`; the script in the bound checkout (`--root` or the binding's root) |
| `brew` | `brew update`; `brew outdated --json=v2`; `brew upgrade --formula <names>`; `brew upgrade --cask <names>`; `brew outdated --json=v2` | each outdated name's installed and newest version | `brew` on the provider path (`/opt/homebrew/bin`) |
| `claude` | `claude update` | `claude --version` | `~/.local/bin/claude` |
| `codex` | `codex update` | `codex --version` | `~/.codex/packages/standalone/current/bin/codex` |
| `opencode` | `opencode upgrade` | `opencode --version` | `~/.opencode/bin/opencode` |
| `skills` | `mise exec -- npx skills update` | none | `mise` on the provider path |

- **agents** installs the agent registry (`stevencarpenter/agents`, DFM-25.C09). When `~/projects/agents/pyproject.toml` exists, the working copy is used as it is: it is never fetched or pulled, the row shows its revision, and a dirty copy has the note `working copy <path> has uncommitted changes; not pulled`. Otherwise the managed copy `~/.local/share/agent-registry` is fast-forwarded. Before any change, a managed copy that is missing (no `.git`), dirty (`git status --porcelain` prints a line), without an upstream branch (`git rev-parse --abbrev-ref @{u}` fails; checked before the fetch) or diverged (after `git fetch`, `git rev-list --count @{u}..HEAD` is not 0) is `blocked` with a note, and nothing is pulled. The updater never merges, resets or clones. Then `scripts/install-agent-registry.sh <copy>` runs from the bound checkout. A missing script is `blocked` before any git command. A changed revision is `updated`; the same revision, and the working copy, is `current`.
- **brew** upgrades every outdated formula and cask, not only declared ones. A name that `brew outdated --json=v2` marks `"pinned": true` (`brew pin`) is left out of the upgrade and its check, with the note `formula node held (brew pin)`. Version-sensitive tools are pinned with mise, per project or globally. `brew upgrade` gets the outdated names, one call per kind, so each formula and each cask is accounted for as itself, also when a formula and a cask share a name. Notes read `formula uv 0.13.5 -> 0.13.6 upgraded` or `cask codex 0.164.0 still outdated (0.165.0)`. An empty list is `current` and runs no upgrade. Homebrew runs with its defaults (the dependents check and cleanup stay on). The manager sets `HOMEBREW_NO_ANALYTICS=1` and `HOMEBREW_NO_ENV_HINTS=1`, and `HOMEBREW_NO_AUTO_UPDATE=1` for every command after `brew update`.
- **A native tool** (`claude`, `codex`, `opencode`) must exist at its owner path. If it does not, it is `blocked` with that path, and no other copy (for example the Homebrew cask) is used. The runner's executable checks apply: a link that leaves the owner's prefix is `blocked`.
- **A second copy.** For each native tool, `brew list --versions` and `brew list --cask --versions` look for a Homebrew copy under Homebrew's name for it: the cask `claude-code` (the `claude` cask is the desktop app), the formula and cask `codex`, the formula `opencode`. The check runs after every updater ran, so the version is the one `brew` left, and each copy found is a line after the rows (`  note: codex second copy: Homebrew cask codex 0.165.0`). It is never removed or chosen.
- **skills** reports no version. Exit 0 is `current` with the note `skills update ran; it reports no version`.

## Source trust

`skills` and `agents` fetch extension sources and run what they fetch. Each source needs a grant in `state/grants.json` before the updater runs (DFM-25, C04; the identities are in docs/sync.md, "Source identities"). `claude`, `codex`, `opencode` and `brew` update a binary or the packages from their owner and are not gated.

| Updater | Sources, read without running a command |
| --- | --- |
| `skills` | `npm:skills`, the installer that `npx` runs, and the `source` and `sourceUrl` of every entry of the `skills` CLI's source record |
| `agents` | the `url` of `[remote "origin"]` in `<copy>/.git/config`, for the copy the updater uses (the working copy or the managed copy) |

- **The `skills` source record** is `~/.agents/.skill-lock.json`, as `skills` 1.7 writes it: `{"version": 3, "skills": {"<name>": {"source": "o/r", "sourceType": "github", "sourceUrl": "https://github.com/o/r.git", ...}}}`. `skills update` reads it for the global skills. The updater's environment is cleared, so `XDG_STATE_HOME`, which would move the record, is not set. The updater runs in the manager's cache directory, which holds no `skills-lock.json` or `.agents/skills`, so `skills update` takes the global scope and not a project's. A missing or unreadable file, invalid JSON, a `version` other than 3, an entry without a text `source`, or a `sourceUrl` that is not text is a record this manager cannot read.
- **The `agents` origin** is read from the file, so no git command runs before the check. A config with an `[include]`, `[includeIf]` or `[url]` section (an `insteadOf` rewrite), or without exactly one origin `url`, cannot be read: the file alone does not say where git fetches from. A copy whose `.git` is a file (a linked worktree) cannot be read either. The origin's grant covers the pull and the registry's own install code.
- Each source without a grant, and each source that maps to no identity (a local path, a well-known skill domain), makes the row `blocked`, and the updater runs no command. One note per source: `E_SOURCE_UNTRUSTED: no grant for npm:skills; review it, then run dfm sync --trust-origin npm:skills`.
- A source record that cannot be read makes the row `blocked` with the cause, for example `cannot read ~/.agents/.skill-lock.json: entity not found; nothing ran`. It is never `current`, and the updater never runs without the check.
- The check runs in the listing before publication and again under the writer lock just before the updater runs, with the grants read under the lock, because the record or the grants can change between them.
- One updater fetches all of its sources, so one ungranted source blocks the whole row.

## Tracked files

An updater must not leave a tracked file changed in a source checkout without the owner knowing (DFM-25, C05).

- Before the first updater, sync records `git status --porcelain=v1 -z --untracked-files=no` (with `--no-optional-locks`, through the scrubbed git of docs/sync.md) for the bound checkout and each nested source checkout that the published lock declares. After each updater that ran, it records it again and compares.
- A tracked file that the updater changed (a new modified, deleted, type-changed or renamed entry) makes that row `failed` with one note per file: `E_CONFLICT: claude changed the tracked file <checkout>/<path> (M); it is left in place, not reverted, staged or committed`. A file that resolves through a dfm link into a checkout shows as the file in the checkout.
- The change stays for the owner. Sync does not revert, stage, commit or push it. The next `dfm sync` refuses an update that would overwrite it (docs/sync.md, step 2).
- The other updaters still run. A `blocked` updater ran no command and is not compared. A status that cannot be read after an updater fails that row with the cause; one that cannot be read before the first updater is an error of the step (`upgrade: failed (<code>)`).

## Statuses

| Status | When |
| --- | --- |
| `updated` | the version changed (brew: every upgraded name left the outdated list) |
| `current` | exit 0 and the same version (brew: nothing outdated) |
| `pending` | a stop request (SIGINT, SIGTERM) ended or prevented it, or the update ran and the new version could not be read |
| `failed` | a command exited nonzero, timed out or was ended by a signal; the updater changed a tracked file in a source checkout (`E_CONFLICT`, see "Tracked files"); brew: a name stayed outdated or the list was not valid JSON |
| `blocked` | skills, agents: a source has no grant or maps to no identity, or the source record cannot be read (see "Source trust"); agents: the install script or `git` is missing, or the managed copy is missing, dirty, without an upstream or diverged (local commits); the tool, or `mise` for skills, is missing from its owner path, fails the executable checks, or its version cannot be read (or is empty) before the update; brew: `brew` cannot be resolved on the provider path |

A failure note gives the command, its exit code and the size of its standard error. The text is withheld (docs/secrets.md); run the command by hand to see it. A partial `brew upgrade` is `failed`, and the notes list each name as upgraded or still outdated.

## Trials

A trial does not hold `brew`. The owner's decision of 2026-10-31 replaces DFM-31.C09: a tool is trialed without an effect on any other update, and it is updated with the other packages as if it were already adopted. `brew` runs as if no trial existed and upgrades a trial's formula or cask with every other outdated package. No updater reads the trial records, so an unreadable record or an interrupted trial write does not change the run. A missing trials directory still refuses the whole run before any scope, as state loss (`E_STORAGE_INVALID`): the store refuses every writer on a missing state directory (docs/state.md). A `mise/v1` trial is rolled by `dfm packages --update`, not by `sync` (docs/toolchains.md, "Trials").

## In sync

1. After the plan, before publication and the scopes, each updater is listed with what it would run and its resolved path, or `blocked: <why>` (DFM-31.C02), the source check included. Every declared updater is listed, a blocked one too (DFM-25, G6). Nothing runs yet.
2. The four scopes run as docs/sync.md describes.
3. A refused run (an error exit) runs no updater. A skipped or blocked scope does not stop them.
4. `--no-upgrade` prints `upgrade: skipped (--no-upgrade)` and runs none.
5. The step also runs after "nothing to change".
6. Under the writer lock, the profile is resolved again from `--root`, because another sync may have changed it since the review. An updater runs only when the review listed it and the profile still declares it; one added meanwhile runs in the next sync, after its review. A profile that declared none in the review takes no lock. Each updater that runs prints one row: `  <status> <name> <before> -> <after>`, then its notes. A blocked updater prints its row too and counts in the summary. After the last row, the second copies of the native tools are read (see "The fixed set").
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
- **Tracked files are compared by status line.** A file that was already changed before an updater and that the updater changes again keeps its status line, so the guard does not see it. A change that another process makes while an updater runs is counted against that updater.
- **Source trust reads recorded sources.** A global git `insteadOf` rewrite in `~/.gitconfig` is not applied to the `agents` origin, and the check reads the `skills` record version 3 only.
- **agents** does not clone a missing managed copy. Clone it by hand. A working copy is installed with its uncommitted changes.
- **Second copies** are looked for in Homebrew only, not in npm or mise.
- **Evidence scope.** Tests use look-alike updaters. No test runs a real updater. The `agents` sync test uses real git against a local registry remote and a look-alike install script.
