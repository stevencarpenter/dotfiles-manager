# Sync upgrades (DFM-31, first slice): design

Status: draft for owner approval. Date 2026-10-27.

## Intent

The owner runs `dfm sync` and expects the tools on the machine to be at their newest versions afterwards. Today a second command does that: the zsh function `burp` (`~/.dotfiles/home/.config/zsh/profile.d/personal-shell-functions.zsh`), which runs `claude update`, `codex update`, `opencode upgrade`, `brew update`, `brew upgrade -y` and `npx skills update`, each independent of the others, and lists the failures at the end. On 2026-10-27 the owner ran `dfm self-update` and `dfm sync` on the M5 and the tools stayed at their old versions: the packages scope had zero operations, because the dotfiles declare no package resources and `packages` never upgrades.

The owner's directive (2026-10-26): dfm manages the dotfiles, full stop, and a plain `dfm sync` just runs. This slice makes `dfm sync` do what `burp` does, so `burp` can be removed.

## Owner decisions (2026-10-27)

- Every `dfm sync` runs the upgrades.
- `dfm sync --no-upgrade` skips them. (The owner asked for a skip flag; the name is the designer's call.)
- The upgrade is wholesale, as in `burp`: `brew upgrade` upgrades every outdated installed formula and cask, not only declared ones. Version-sensitive tools are pinned by mise at the project or global level, so a wholesale upgrade is safe for this owner.
- Each profile in the dotfiles declares which updaters its machine runs.
- The updaters are a fixed set built into dfm, selected by name. A dotfiles commit cannot add a command. A free command string was rejected, because a commit pulled from GitHub could then run any command with no approval.
- An upgrade needs no approval and no flag. The three gated kinds (new origin, takeover, service restart) are unchanged.

## Goals

1. A profile declares `updaters = [...]` with names from the fixed set.
2. `dfm sync` runs the declared updaters after the four scopes (config, packages, secrets, services).
3. Each updater is reported as `updated`, `current`, `pending`, `failed` or `blocked`, with the versions before and after where the tool reports one. No updater is reported `updated` unless a changed version is observed (DFM-31.C12).
4. A failure or a missing tool in one updater does not stop the others.
5. A second copy of a tool under another owner is reported with both paths (DFM-31.C13). It is never removed or chosen silently.

## Non-goals

- Git sources, pins and locks (DFM-31.C01, C06). Sync already fast-forwards tracking sources.
- One combined diff and approval before the upgrade (DFM-31.C04). It conflicts with the "just runs" directive. The upgrade has no plan rows and is not in the sync digest.
- Upgrading mise tools globally. `packages --update` (docs/toolchains.md) stays as it is and sync still runs `packages` without `--update`.
- Moving the Brewfile into dfm package resources. That is a separate DOT task. The wholesale brew upgrade does not need it.
- Plugin and extension updates beyond `npx skills update` (DFM-25).
- A standalone `dfm upgrade` command.
- Linux, the i9, or any platform other than native Apple Silicon macOS.

## Anti-goals

- A sync that reports `result: ok` with no sign that an updater failed or was skipped.
- A dotfiles change that makes sync run a program the manager does not know.
- Removing, unlinking or choosing between two copies of a tool.
- A failed upgrade that blocks the next sync (for example, an unresolved journal entry). A rerun of an updater is its recovery.

## The fixed set

| Name | Commands, in order | Version read before and after | Expected owner path |
| --- | --- | --- | --- |
| `brew` | `brew update`, then `brew outdated --json=v2`, then `brew upgrade <formula and cask names from the outdated list>` | the outdated list gives each name's installed and current version; after the upgrade, `brew outdated --json=v2` again | `/opt/homebrew/bin/brew` |
| `claude` | `claude update` | `claude --version` | `~/.local/bin/claude`, a link into `~/.local/share/claude/versions/` |
| `codex` | `codex update` | `codex --version` | `~/.local/bin/codex`, a link into `~/.codex/packages/standalone/` |
| `opencode` | `opencode upgrade` | `opencode --version` | `~/.opencode/bin/opencode` |
| `skills` | `npx skills update` | none: the result is `updated` or `current` only if the tool's output says so, else `ran` | `npx` from the mise shims (`~/.local/share/mise/shims/npx`) |

`brew upgrade` gets an explicit list of names, not a bare `brew upgrade`, for two reasons. The before and after lists account for each package (DFM-31.C07). An active trial's package can be left out (DFM-31.C09, see below). An empty outdated list means `current`, and `brew upgrade` is not run.

Each updater's executable is resolved at its expected owner path. If that path does not exist, the updater is `blocked` with the path in the reason, and the others still run (DFM-31.C02). The manager never falls back to another copy found on `PATH`.

**Second copies.** Before an updater runs, the manager checks the other known owners of the same tool and reports each copy it finds as a note on that updater's row. For `codex`, that is the Homebrew cask (`/opt/homebrew/Caskroom/codex`), which exists on the M5 today. The `brew` updater upgrades that cask like any other outdated cask. The note gives both paths and both owners.

## Profile schema

The profile document (`crates/model/src/resolve/docs.rs`, `ProfileDoc`) gets one optional field:

```toml
updaters = ["claude", "codex", "opencode", "brew", "skills"]
```

- Absent or empty: no updaters run, and the sync output says so in one line.
- Each value must be a name from the fixed set. A name not in the set, or a name given twice, is `E_MANIFEST_INVALID` with the profile path and the index, the same as an unknown module.
- The updaters run in the declared order, one at a time.
- The field stays at `schema = 1`, because it is optional and a manager without it rejects unknown fields today. (The builder must confirm how the current parser treats an unknown field. If an older manager would accept and ignore it, document that.)

## Sync behavior

1. The four scopes run as today: one review, the per-scope receipts, warnings collected.
2. If `--no-upgrade` is given, the upgrade step prints `upgrade: skipped (--no-upgrade)` and does nothing else.
3. If a scope refused the run (an error exit), the upgrade step does not run.
4. If a scope was blocked or skipped with a warning (for example, secrets when 1Password is locked), the upgrade step still runs. This matches the owner's "run others, warn" choice.
5. Each declared updater runs and gets one output row: name, status, versions before and after, and notes (second copies, the blocked reason, the last stderr line on a failure).
6. The `scopes:` summary gets an `upgrade:` line with the counts per status.
7. Each `failed`, `blocked` or `pending` updater adds a warning at the end of stdout. The exit code stays 0, the same as a blocked scope (owner decision for DFM-63, applied here by the designer).
8. `--preview` lists the updaters that would run and their expected paths, and runs none of them.

The upgrade step writes one receipt (`command = upgrade`) with each updater's row. It writes no journal intent: an interrupted updater leaves the tool as its own updater left it, and the next sync runs it again.

## Process rules

- Commands run through the existing `Runner` (`crates/engine/src/proc.rs`): a fixed search path, an environment built from empty, the real `HOME`, a timeout and a process group.
- Each updater may need its own variables (for example, network proxy variables). Pass through only the variables an updater is known to need. Do not pass the caller's whole environment.
- `brew` keeps `HOMEBREW_NO_ANALYTICS` and `HOMEBREW_NO_ENV_HINTS`, and drops `HOMEBREW_NO_AUTO_UPDATE` only for `brew update` itself.
- Timeouts: 30 minutes for `brew upgrade`, 10 minutes for each other command (the designer's call). A timeout is `failed` with "timed out after N minutes".
- Updaters run detached, with no terminal and stdin closed (owner decision, 2026-10-27). A step that needs a password or a prompt, for example a cask whose pkg installer runs `sudo`, fails. The `brew` row names that cask, and the warning tells the owner to upgrade it by hand. So an agent can run `dfm sync` unattended.
- A stop signal (SIGINT) during an updater stops that updater the way `Runner` stops any child, skips the rest, and reports them as `pending`.

## Trials (DFM-31.C09)

Superseded 2026-10-31 by the owner's decision: "I want to trial tools without it impacting any other brew update or other package manager update. It is specifically being trialed, and it should also get updated with the other packages like it were already adopted because that's the whole point! We will build purge immediately after this." A trial no longer holds `brew`, and `dfm packages --update` rolls an active `mise/v1` trial's tool (docs/upgrade.md, "Trials"; docs/toolchains.md). The text below is history.

Revised 2026-10-27 after review: the exclusion below does not protect a trial, because `brew upgrade` also upgrades dependencies and dependents of the names it gets. The built rule: while a Homebrew trial is active or a trial record cannot be read, `brew` is `blocked` and runs no command (docs/upgrade.md, "Trials"). The original text follows.


If an active trial record under `state/trials/` names a Homebrew package, the `brew` updater leaves that name out of `brew upgrade` and reports it as `held (active trial <id>)`. No other updater reads the trial records. None are active on the M5 today.

## Acceptance for this slice

These map to DFM-31 criteria. The rest of DFM-31 stays open.

- C02: a missing tool or an unreadable version is reported before or in place of its run, and does not stop the others.
- C03: the default is rolling, with no soak and no hold. A hold exists only for an active trial in this slice. Superseded 2026-10-31: no trial holds an updater (see "Trials").
- C07: every declared updater ends in exactly one status.
- C08: tests prove that a failed updater, a missing tool, a version that does not change after the update (`pending` or `current`), and a stop mid-run cannot produce an all-`updated` summary or a missing warning.
- C12: `claude` and `codex` run through their own updaters, and `updated` needs a changed `--version`.
- C13: a second copy is reported with both paths.
- C09: the trial exclusion above. Superseded 2026-10-31 by the owner's decision quoted in "Trials".
- Tests use look-alike executables for every updater, as `crates/cli/tests/toolchains.rs` does for mise and uv. No test runs a real updater.
- The docs: a new `docs/upgrade.md`, and updates to `docs/sync.md` and `README.md`.
- `just check` passes on the branch head.

## Builder check resolutions (designer's calls, 2026-10-27)

- **Executable resolution.** `Runner::resolve` refuses a link whose target leaves the search directory's parent (`crates/engine/src/proc.rs:161-167`), and a mise shim picks its tool from argv0, which `Runner` does not set. The check stays as it is. Each updater resolves at the directory its owner installs into: `codex` at `~/.codex/packages/standalone/current/bin`, and `skills` runs as `mise exec -- npx skills update` with `mise` from `/opt/homebrew/bin`. The table's "expected owner path" column means these directories.
- **Exit 0 and an unchanged version** is `current`, not `pending`. `pending` is only for a stop mid-run, or a tool that says in its output that the new version starts on next launch.
- **`skills`** has no `ran` status. Exit 0 is `current` with the note "skills update ran; it reports no version". Exit non-zero is `failed`.
- **Receipt.** No new receipt fields and no new status, so a rolled-back manager can still read every receipt (all receipt structs use `deny_unknown_fields`). The receipt holds one operation per updater with an existing status. The versions and notes go to stdout only. A receipt with a `failed`, `blocked` or `pending` updater records outcome `failed` with exit 1, as `state.rs:773-777` requires, while sync itself exits 0.
- **Doctor.** The last-run row (`doctor_cmd.rs:586-596`) skips `command = upgrade` receipts. A separate `upgrade` row reports the latest upgrade receipt, with the fix "Run dfm sync". A failed updater then does not hide the result of the last configuration run.
- **Homebrew variables.** `brew upgrade` runs with Homebrew's defaults, as in `burp`: the dependents check and cleanup stay on. Only `HOMEBREW_NO_ANALYTICS` and `HOMEBREW_NO_ENV_HINTS` are set. The packages adapter's `brew_env()` is not reused.
- **A partial `brew upgrade`** (exit non-zero, some names changed) is `failed`. Stdout lists each package as upgraded or still outdated.
- **Second copies.** For `claude`, `codex` and `opencode`, the check looks for a Homebrew formula or cask of the same name and reports it. Other owners (npm global, mise) are not checked in this slice.
- **Profile schema.** A manager without this change refuses a profile that has `updaters` (`E_FIELD_UNKNOWN`, `docs.rs:219-230`). The hint "a new key needs a new schema version" changes to name optional keys. The DOT change that adds `updaters` to a profile lands only after every machine that uses that profile runs the new manager.
- **"Nothing to change".** The upgrade step also runs after sync's "nothing to change" return (`sync_cmd.rs:1958-1984`). It does not run with `--preview`.
- **Output size.** `brew upgrade` gets a raised `max_stdout`, so a large upgrade does not fail with `E_LIMIT_EXCEEDED`.

## Left to the builder

- Module layout (for example, `crates/cli/src/adapters/updater.rs` and a `upgrade` step in `sync_cmd.rs`).
- The exact output format of the rows, within the existing plan and summary style.
- How `skills` recognizes "updated" or "current" in `npx skills update` output. If no stable marker exists, report `ran`.
- Whether the version read uses `--version` output as one string or a parsed version. A changed string is enough.

## Follow-ups outside this slice

- DOT: remove `burp` after this ships and the M5 runs it.
- DOT: declare `updaters` in `personal-mac.toml` (and later the work M3 profile).
