# Adopt (DFM-33): design

Status: approved by the owner on 2026-10-28.

## Intent

The owner tries a tool, configures it in its live location until they like it, and then wants it in the dotfiles repo as a normal dfm-managed tool. Today this last step is done by hand: worktrunk and pi were adopted by writing `fs-link/v1` resources into `dfm-module-core.toml` and copying files into `home/`. `dfm adopt` does that step from a reviewed trial record, with the same preview, digest and receipt rules as the other commands.

The first real case is tuicr. It reads `$XDG_CONFIG_HOME/tuicr` or `~/.config/tuicr` (`config.toml` and `themes/`), from strings in the 0.27.0 binary. It is installed today through a mise shim, which the owner used to keep it apart from the managed tools. Going forward the owner wants it from Homebrew (`homebrew/core/tuicr`, 0.28.0, bottled). It has no configuration file yet. The owner will configure it beyond the defaults by editing the live file.

## Owner decisions (2026-10-28)

- The flow is "edit the live file, then pull it into the repo". The repo file is never the starting point.
- The owner removes the mise copy of tuicr by hand first, then runs `dfm trial tuicr --provider homebrew/v1`. Adopt does not handle a switch between providers.
- The target module is named on the command line with `--module <name>`. It is required. tuicr goes to `core`.
- Modules stay separate for a reason. `atuin` has its own module so that work machines never sync shell history. Adopt never merges modules and never suggests a merge.
- The install is recorded as a line in the Brewfile, not as a dfm package resource. Every package is in `Brewfile` or `Brewfile.<identity>` today, and brew stays first-class (owner decision of 2026-10-15). The 2026-10-27 sync-upgrades design already lists "Moving the Brewfile into dfm package resources" as a separate task.
- Adopt never commits or pushes. The owner reviews, commits and pushes.

## The flow for tuicr

1. The owner removes the mise copy of tuicr.
2. `dfm trial tuicr --provider homebrew/v1` installs it and records `~/.config/tuicr` as absent in `baseline.paths`. (The default candidate is `~/.config/<tool>`.)
3. The owner edits `~/.config/tuicr/...` as often as they like.
4. `dfm adopt trial-tuicr --module core --preview` shows what adopt will do and prints a digest.
5. `dfm adopt trial-tuicr --module core -y <digest>` does it.
6. The owner reviews the repo diff, commits and pushes.

## The command

```
dfm adopt <trial-id> --module <module> [--root <dir>] [--preview] [-y|--yes-plan <digest>]
```

`--root` defaults to the bound root, as for `apply`. The machine must be bound. Exit codes follow `trial`: 0 done, 1 a failure after the first write, 2 usage, 3 blocked before any write (approval missing, stale digest, refusal), 130 and 143 a stop request.

## What adopt does

### Candidates

The candidates are the regular files under each `baseline.paths` entry of the trial record, as they exist now:

- An entry with `existed: false` (the tuicr case): every regular file now under that path is a candidate.
- An entry with `existed: true` and a `sha256`: the file is a candidate only when its hash changed since the baseline. An unchanged pre-existing file is not the trial's work.
- An entry with `existed: true` and no hash (a directory that existed before): the baseline has no per-file hashes for a directory, so adopt cannot tell the trial's files from older ones. Adopt lists every regular file under it as a candidate and marks the entry `pre-existing directory` in the preview. The owner's approval of the digest covers them.

Paths outside the baseline are never candidates. In particular, `~/Library/Application Support/tuicr/reviews/` holds tuicr's review sessions and must never be adopted. The trial's `unknown` token `paths-outside-baseline` is printed in the preview as a reminder, as `trial` prints it.

Adopt works on files. It never declares a directory link (`directory = true`), because a directory link would also capture files the tool writes later (caches, state).

### Refusals (exit 3, nothing written)

- The trial id has no record, or the record cannot be read.
- The trial is already adopted.
- The module manifest is not inside the `--root` git tree. This refuses `core` in the planned work-host layout (DFM-29), where `core` lives in a nested, pinned base checkout. A module of the work overlay itself is inside the root and is accepted.
- `--module` names a module that the bound profile does not select, or one that the bundle does not declare. The message names the profile file and the `modules = [...]` line to edit (DFM-33.C07, C08). `core` is always selected through the bundle's `core =` key.
- A candidate is a link, a special file, unreadable, or over 16 MiB (the `trial` baseline limits).
- A candidate fails the secret check. Adopt runs `gitleaks dir` over the candidate files through the runner (owner decision 2026-10-28). The store's own secret rules are not enough: they match known values only, and release builds have an empty list (docs/state.md rule 11). A finding refuses the run and names the path, never the value. A missing `gitleaks`, or a scan that cannot complete, blocks adopt with `E_PROBE_FAILED`. The owner can move the secret into a 1Password template (`op-template/v1`) by hand and run adopt again. The dotfiles lefthook `gitleaks git --staged` hook stays as the second check at commit.
- A candidate target is already owned by a resource in any module of the composition, or by a trial's `owned_outputs` (DFM-33.C03).
- A repo source path that adopt would write already exists in `--root`, tracked or untracked. Adopt never overwrites a repo file.
- The module manifest or the Brewfile that adopt edits has uncommitted changes. This keeps adopt's edit as its own reviewable diff.
- The composition with the new resources does not resolve: adopt resolves the profile against the edited manifest in memory before any write (DFM-33.C03).
- A changed candidate, manifest, Brewfile, trial record, binary or binding between the preview and the approval: `E_PLAN_STALE`.

### Writes, in order, under the writer lock

1. **Repo files.** Each candidate is copied to `<root>/home/<HOME-relative path>` (for tuicr: `home/.config/tuicr/config.toml`), with the live file's mode. This matches how worktrunk's file is laid out.
2. **Module resources.** For each candidate, one `fs-link/v1` resource block is appended to the module manifest, in the form the manifests already use:

   ```toml
   [[resources]]
   id = "config-tuicr-config-toml"
   kind = "link"
   adapter = "fs-link/v1"
   target = ".config/tuicr/config.toml"
   [resources.value]
   source = "home/.config/tuicr/config.toml"
   ```

   The id is derived from the target path, as the existing ids are. Adopt appends whole blocks at the end of the file and never rewrites existing text. A prior hand edit of these manifests with string replacement broke headers and line endings (2026-10-22). After the append, adopt parses the file again and checks that the earlier bytes are unchanged.
3. **Brewfile line.** When the trial installed or found a `homebrew/v1` package, adopt appends `brew "<name>"` (or `cask "<name>"`) to a Brewfile in `--root`, when no line for that package is there. dfm has no identity concept, so the rule uses the module name: `Brewfile.<module>` when that file exists (today only `Brewfile.personal`), else the shared `Brewfile`. A package that only mise lists gets no line, and the preview says so.
4. **Take over the live files.** Each live file is replaced by a link to its new repo source. The planner has no such operation: a regular file at a link target is `E_CONFLICT` even when its content equals the source (docs/planner.md rule 5), and apply never adopts a regular file (docs/apply.md). Adopt adds a new adopt-only operation (owner decision 2026-10-28). Only `dfm adopt` plans it, and only for a live regular file whose hash equals the repo copy adopt wrote in the same run, rechecked under the lock immediately before the replace. `apply` and `sync` keep refusing regular files, and their rules and docs do not change. The operation is journaled and receipted through the publish path that `apply` uses, not a second publisher. The adopt digest is its approval, so no separate `--allow-adopt` is needed. Ownership is recorded in the ledger against the new resource ids.
5. **End the trial.** The trial record is marked adopted. The record format change and the field name are the builder's call. Every reader that treats a trial as active must treat an adopted trial as ended. The `brew` updater must stop waiting for it (`crates/cli/src/adapters/updater.rs`, "brew waits for ... end the trial"). A later `purge` (DFM-34) must refuse an adopted trial.
6. **Receipt.** `command = adopt`, with the plan digest, one operation per written path, and the trial id.

The output ends with the list of repo paths that adopt changed, so the owner can review them with `git diff` and `git status`.

### Interruption

Adopt writes the trial's "adoption started" intent to the journal before the first repo write. An interrupted run leaves the repo files and manifest edits in place and the trial not adopted. A second run must detect its own partial work by content: a repo source that equals the live file, and resource blocks that match what it would write. It must complete the run, not refuse it as an existing repo file. The exact recovery rule is the builder's call, within docs/state.md: never infer deletion authority, and never blindly replay.

## After adopt

- `dfm sync` before the owner pushes: sync copies the dirty checkout into staging (docs/sync.md, "Staging is a copy of the checkout"), so it sees the new resources. The adopted links stay, and no removal is planned.
- `dfm sync` after the push, on this machine and on a fresh one: the resources resolve from the repo, and the links are current or published. dfm is the single owner (DFM-33.C05). On a fresh machine, `scripts/brew-sync.sh` installs tuicr. `dfm sync` does not install Brewfile lines: its `brew` updater runs `brew upgrade` only.

## Goals

1. `dfm adopt <trial-id> --module <m>` promotes a trial's configuration files into the dotfiles repo as `fs-link/v1` resources of the named module, and its package into the matching Brewfile.
2. Preview and digest approval, as for `trial` and `apply`.
3. The live files become dfm links with recorded ownership. The trial ends.
4. Nothing is committed or pushed.
5. A later `dfm sync` reproduces the result with one owner.

## Non-goals

- Adopting files outside the trial's baseline, or adopting without a trial.
- A provider switch (mise to brew). The owner removes the old copy by hand.
- Tools installed by a downloaded script (`curl ... | sh`). Trial has no provider for them yet. A separate research ticket covers script-based install trials and self-updating tools.
- Declaring dfm package resources in the modules.
- Creating a new module or editing the profile's module list. Adopt names the line to edit.
- Generators, merges and secret templates as adopt outputs. Adopt writes `fs-link/v1` only.
- Retroactive adoption of tools already in the repo (worktrunk, pi).
- `purge` (DFM-34).

## Anti-goals

- An adopt that captures application data (review sessions, caches, history) because it sat next to the config.
- An adopt that rewrites or reformats existing manifest or Brewfile text.
- An adopt that commits, pushes, or changes the profile.
- An adopt that silently overwrites a repo file or a file owned by another module.
- Secrets in the repo.

## Tests (DFM-33.C06)

Through the installed CLI, with the `brew` and `mise` look-alikes in `crates/cli/tests/common/providers.rs`:

- The tuicr shape: an absent baseline directory, a config file created after the trial, `--module core`. Check the repo file, the appended block, the Brewfile line, the link, the ledger owner, the ended trial and the receipt. A following `dfm apply --preview` plans the link as current.
- A secret in a candidate file: refused, zero writes, with a `gitleaks` look-alike that reports a finding. A missing or failing `gitleaks` also blocks.
- `apply` and `sync` still refuse a regular file at a link target whose content equals the source.
- A module the profile does not select, and a module the bundle does not declare: refused with the profile line (C07, C08).
- A forbidden override of a core resource: a target that core already owns is refused (C03).
- Missing source access: the root is missing or not writable. Refused before any write.
- A malformed module manifest: refused before any write.
- Concurrent edits: the live file or the manifest changes between the preview and the approval (`E_PLAN_STALE`), and two adopts run at once (writer lock).
- Each handoff boundary: a stop after the repo copy, after the manifest append, after the Brewfile append, and after the takeover. A second run completes. No path is deleted.
- An unchanged pre-existing baseline file is not a candidate. A changed one is.
- The `brew` updater no longer waits for an adopted trial.

## Builder's calls

- The trial record field or file move that marks it adopted, and any schema bump.
- The recovery rule for a second run after an interruption.
- The id rule for generated resource ids. It must reproduce the existing pattern (`config-worktrunk-config-toml` for `.config/worktrunk/config.toml`).
- Where in `crates/cli` the command lives. It should reuse `trial_cmd.rs` patterns and the `apply` publish path rather than add a second publisher.
- The text of the preview lines.

## Designer's calls (owner can change them)

- Files only, never directory links.
- An unchanged pre-existing baseline file is not a candidate.
- The Brewfile choice by module: `Brewfile.<module>` when it exists, else the shared `Brewfile`. No identity field is added to dfm.
- A module manifest outside the `--root` git tree is refused. Adopt into a nested pinned base checkout is not supported.
- Refusal of an uncommitted module manifest or Brewfile.
- The adopt digest approves the takeover of the live files, with no extra `--allow-adopt`.
- A missing `gitleaks` blocks adopt rather than skipping the scan.
