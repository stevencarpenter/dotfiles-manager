# Purge design (DFM-34)

Status: owner request of 2026-10-31, "build out the trial purge functionality to remove a tool that is under trial". The owner skipped the remaining questions ("use your best judgement and continue"). Every choice below that the owner did not state is marked as the builder's call.

## Intent

The owner's words: "I would expect the package to be uninstalled and all of its data files removed as well. The idea of trialing is that we install the tool through the medium we chose, and keep track of where it installs, what folders and files it owns, any configuration files, and if we adopt it those pull into the configuration permanently. If we purge they are all removed."

`dfm purge <trial>` is the second end of a trial (the first is `adopt`). It removes the package the trial installed and the files and folders the tool created at the paths the trial recorded. It never removes what existed before the trial.

## Goals

1. Uninstall the trial's package by name, every installed version, when the trial installed it.
2. Uninstall the Homebrew dependencies the trial's install added, when nothing else needs them.
3. Remove each recorded configuration and data path that was absent before the trial and is present now, files and whole directories.
4. End the trial: remove its record, so the tool can be tried again.
5. Preview, digest and `--yes-plan` approval, journal entries and a receipt, as `trial` and `adopt` do.

## Anti-goals

- Removing a tool or a path that existed before the trial.
- Following a link, or removing through a parent directory that is a link or that another user owns.
- `brew autoremove`, `brew uninstall --ignore-dependencies`, `--zap`, or any removal that is not named by the record.
- Claiming the machine is clean. Effects outside the recorded paths (`unknown`) are reported as retained.

## Data paths in the trial baseline (builder's call)

Today the baseline has configuration candidates only. To meet "all of its data files removed", `trial` also observes these data candidates for the tool, before the install:

- `~/.local/share/<tool>`, `~/.local/state/<tool>`, `~/.cache/<tool>`
- `~/Library/Application Support/<tool>`, `~/Library/Caches/<tool>`

They go in a new optional field `baseline.data` (same shape as `baseline.paths`), so `adopt`, which promotes `baseline.paths`, never promotes a cache. The digest covers them. The field is optional, so older records read. A record with the field is schema 4 (builder's call after review), so an older binary reports it as newer instead of malformed. A trial recorded before this change has no data baseline, and purge says so.

## Purge decision

Inputs: the trial record, the providers' probes, the live paths, the other active trials.

- **Adopted trial:** refused, `E_CONFLICT`.
- **Package:** uninstall when `package.install` is present and the provider lists the package. A pre-existing tool (`tool_preexisting`), a package another active trial names, or a package only another owner has is retained with the reason. An unlisted package needs no action.
- **Dependencies (Homebrew only):** after the package, the recorded dependencies that are still installed and that no other active trial names are uninstalled in one `brew uninstall --formula` call, so a dependency of a dependency goes too. When Homebrew refuses because an installed formula still needs one, each is tried alone once; a refused one is retained with the reason, not a failure. Any other brew failure is a failure.
- **Paths (`baseline.paths` and `baseline.data`):**
  - `existed: true`: retained ("existed before the trial"), changed or not.
  - `existed: false`, absent now: nothing.
  - `existed: false`, present now: removed. The preview shows the kind and, for a directory, its entry count and bytes. The digest covers a fingerprint of the tree (relative path, kind, size of every entry), so a change between preview and run is `E_PLAN_STALE`.
  - A link at the path, a parent that is a link or another user's, an entry that is not UTF-8, or a tree over the entry cap is retained with the reason.
- **Unknown effects:** printed as retained.

## Run

Order and evidence follow `trial`: preview, `--preview` stop, `E_APPROVAL_REQUIRED` without `--yes-plan`, the writer lock, a second decision that must match the digest, then each action with an `intent` and a `done` or `failed` journal entry under one `op_id` (`purge-<ns>-<pid>`), and one receipt (`command = purge`). When every action succeeded, the record is removed last. When one failed, the record stays and a later `dfm purge` continues from the live state; the exit is 1.

## Limits (builder's call)

- Purge reads no source tree, so it cannot see a module that declares the same package by hand. `sync` would reinstall it.
- A mise tool is uninstalled with `mise uninstall --all <tool>`, or for a pinned trial only the installed versions its pin matches. mise has no dependencies to remove.
- The data candidates are a fixed list by tool name. A tool that writes elsewhere needs `--config` at trial time.
