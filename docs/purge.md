# Purge

The `purge` command (DFM-34, `purge <trial>` in DESIGN.md section 7) ends a trial that is not adopted. It removes what the trial added: the package, the Homebrew dependencies that its install added, and the configuration and data paths that were absent before the trial. Then it removes the trial record, so the tool can be tried again. The trial and its record are in [docs/trial.md](trial.md). The design is `docs/superpowers/specs/2026-10-31-purge-design.md`.

| Part | Location |
| --- | --- |
| `purge` command: arguments, decision, digest, actions, receipt | `crates/cli/src/purge_cmd.rs` |
| Provider uninstalls (`uninstall`, `uninstall_formulae`) | `crates/cli/src/adapters/package.rs` |
| Record removal (`Store::remove_trial`), tree removal (`Dir::remove_tree`) | `crates/engine/src/store.rs`, `crates/engine/src/fsys.rs` |
| Tests with the `brew` and `mise` look-alikes | `crates/cli/tests/trial.rs`, section "purge" |

## The command

```
dfm purge <trial-id> [--preview] [-y|--yes-plan <digest>]
```

`<trial-id>` is the record's id. A tool name also works when the trial has the default id `trial-<tool>` (`dfm purge atuin`).

## What purge does with each item

| Item | Action | Kept when |
| --- | --- | --- |
| The package (`package.adapter`, `package.name`) | `brew uninstall --formula <name>` or `--cask <name>`, or `mise uninstall --all <tool>`: every installed version. A listing after the uninstall must not name it. | The tool existed before the trial (`tool_preexisting`). Another active trial names the package (Homebrew names compare by formula: `atuin` and `homebrew/core/atuin` are one package). The provider's listing shows another owner. |
| The Homebrew dependencies (`package.install.dependencies`) | One `brew uninstall --formula <dep>...` call for all of them, after the package. Homebrew checks dependents against the whole set. When it refuses, each dependency is tried alone once. | Another active trial names it. Homebrew refuses it because an installed formula still needs it (its error says "because it is required by"). That is a kept item (`current`), not a failure. Any other brew failure (its lock, a permission, a timeout) fails the run and keeps the record. A name that starts with `-` is refused before brew runs. |
| A recorded path (`baseline.paths`, `baseline.data`) that was absent before the trial and is present now | Removed: a file, or a directory with its whole tree. Each parent is opened as a real directory owned by this user (`Dir::open_below`), and nothing below is followed. | The path is a link or a special file. A parent is a link, a file or another user's. A directory in the tree is another user's, writable by group or others, or not writable by its owner, so the removal could not finish. A module in the ledger, or another active trial, records the path or a path below it. The tree holds more than 100,000 entries or a name that is not UTF-8. It cannot be read. |
| A recorded path that existed before the trial | Nothing. | Always: "existed before the trial", changed or not. |
| `unknown` effects | Nothing. | Always: printed as `retained:`. |

Purge asks only the provider's own listing (Homebrew's Cellar and Caskroom, `mise ls --installed`), not the install-planning checks of `packages`. A listing that cannot answer refuses the whole purge with `E_PROBE_FAILED` before any change. So does another trial record that cannot be read (`E_STATE_INVALID`): it could name the same package or path. Removing the record then would lose the only evidence of what the trial installed.

## Order of one run

1. **Decision.** The record is read (`E_STATE_INVALID` when it is missing or unreadable, `E_CONFLICT` when it is adopted). The providers are probed, the paths are observed and the other active trials are read. The preview prints one `action:`, `keep:` or `nothing:` line per item, the `retained:` line and `plan digest:`. The digest covers every item and its step, the fingerprint of each present path (a file's SHA-256, or its size above 16 MiB; a directory's entry count, bytes and the SHA-256 of every relative path, kind and size), the binary identity and the binding.
2. **Approval.** `--preview` stops here (exit 0). Without `--yes-plan` the run stops with `E_APPROVAL_REQUIRED` (exit 3).
3. **Under the lock.** The writer lock is taken and the decision is made again. A digest that differs is `E_PLAN_STALE` (exit 3, nothing changed).
4. **Actions.** Each action has its own journal operation id, `purge-<ns>-<pid>-<n>`, with an `intent` before it and a `done` or `failed` entry after it. A stop request starts no new action. A failed package uninstall ends the actions: its dependencies and its files stay while the package may still be there. Another failed action stops nothing that follows. A recorded path below a directory that this run removed went with that tree and gets no action of its own. Each removal intent names its path, so `dfm state recover` can settle an interrupted removal.
5. **The record.** When no action failed, the record is removed last: `trial: state/trials/<id>.json removed`. When one failed, the record stays and the run exits 1. Run `dfm purge <id>` again after the fix: the second run decides from the live state, so done steps show as `nothing:`.
6. **The receipt.** `command = purge`, one operation per action, `plan_digest` and a `yes_plan` approval.

| Exit | When |
| --- | --- |
| 0 | Every action succeeded or was kept, and the record is removed. |
| 1 | An action failed after a change. The record stays. |
| 2 | Usage. |
| 3 | Blocked before any change: approval missing, stale digest, missing or adopted trial, probe failure, lock held, not bound. |
| 130, 143 | A stop request. |

## Known limits

- **Data paths are a fixed list.** `trial` observes `~/.local/share/<tool>`, `~/.local/state/<tool>`, `~/.cache/<tool>`, `~/Library/Application Support/<tool>` and `~/Library/Caches/<tool>` (docs/trial.md). A tool that writes elsewhere needs `--config` at trial time. A trial recorded before this change has no data baseline. Its preview prints a `note:` line, and only its configuration paths are covered.
- **No restore.** A path that existed before the trial is kept as it is now. The trial keeps a hash, not the content, so purge cannot restore an earlier version.
- **No source tree.** Purge reads no module. A package that a module also declares by hand is uninstalled, and the next `sync` installs it again.
- **Not an OS sandbox.** Services, logs and paths outside the recorded ones are `unknown` and are never cleaned.
- **Dependency of a kept dependency.** When Homebrew refuses the combined call, each dependency is tried alone once. A dependency that only another trial dependency needs can then be kept, and it is reported.
- **Evidence scope.** Tests use the `test-isolation` build and the `brew` and `mise` look-alikes. No test runs a real uninstall.
