# DFM-78: script-based install trials and self-updating tools

Status: approved by the owner on 2026-11-03 (DFM-78.C08). Kaneo: DFM-78. Inputs: the code at main bc23ea2, `~/.dotfiles` as bound on the personal M5, and the installer scripts fetched (not run) on 2026-11-03. This is a design. Implementation is a separate task.

## Intent

Many tools install only through a downloaded script (`curl -fsSL <url> | sh`). `dfm trial` knows `homebrew/v1` and `mise/v1` only, so such a tool cannot be trialed, adopted (DFM-33) or purged (DFM-34). The owner wants a script install to be a first-class trial path.

A script decides its own write paths, edits shell startup files, downloads more code that nothing pins, and can prompt on `/dev/tty`. On this Mac, every shell startup file that zsh reads is a dfm link into `~/.dotfiles`: `~/.zshenv` and the files under `$ZDOTDIR` (`~/.config/zsh`). So any installer that edits zsh startup files writes into the dotfiles repository through a link. The inventory below shows that four of the five installers would do this here, not only boat.

## Inventory (C01)

Each script was fetched with `curl -fsSL` into its own empty directory and read as data. None was run. Tags: script line (S), vendor docs (D), this Mac (M).

| Tool | Script sha256 (2026-11-03) | Further download | Binary | Shell startup edits | Opt-out | Prompts | Uninstall | Self-update |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| boat `https://boat.dev/install` | `aead8db5…8383` | latest `prod` build, no checksum (S 4-25) | `~/.ascii/bin/boat`, fixed (S 6); links `~/.local/bin/boat`; `/usr/local/bin/boat` only when writable, never `sudo` (S 175-199) | shell from `basename $SHELL`, ZDOTDIR ignored; PATH block prepended and `boat()` appended to `~/.zshrc`; `~/.zshenv` rewritten with `cat tmp > file`, then a PATH block appended (S 42-160) | none | `boat onboard < /dev/tty`, installer exits with its status (S 235-253) | none | `boat self-update`; automatic, at most every 6 hours after a command ends; `BOAT_NO_UPDATE=1` or `--no-update` (D) |
| Claude Code `https://claude.ai/install.sh` | `3a68d340…a944` | manifest from `downloads.claude.ai`, binary checked against its SHA-256; manifest signature not checked (S 149-219) | `~/.local/bin/claude` link to `~/.local/share/claude/versions/<ver>` (D) | none (D) | n/a | `claude install` draws a TUI (S 232) | remove the two paths (D) | background check; `claude update`; `DISABLE_UPDATES` blocks all (D) |
| opencode `https://opencode.ai/v2/install` | `1cde9fc3…0136` | npm tarball, version from the vendor API, no checksum (S 30, 179, 212) | `~/.opencode/bin/opencode`, fixed (S 69) | first existing of `${ZDOTDIR:-$HOME}/.zshrc`, `.zshenv`, then `$XDG_CONFIG_HOME/zsh/*`; only when the directory is not on PATH (S 397-444) | `--no-modify-path` | none | none | `opencode upgrade`; config `update` = `auto`, `notify` (default) or `disable` (D) |
| uv `https://astral.sh/uv/install.sh` | `d263ee68…2db4` | pinned 0.13.7 with embedded SHA-256; the check is skipped when `sha256sum` is missing (S 27, 231, 516) | first of `UV_INSTALL_DIR`, `$XDG_BIN_HOME`, `$XDG_DATA_HOME/../bin`, `~/.local/bin` (S 1226-1316) | every shell family: `~/.profile` (created), each bash rc, first of `$ZDOTDIR/.zshrc` or `.zshenv`, fish `conf.d` (S 1405-1575); PATH test is a text match | `UV_NO_MODIFY_PATH=1` | none | remove the binaries (D) | `uv self update`, which re-runs the installer and its startup edits (D) |
| rustup `https://sh.rustup.rs` | `9502b514…afc0` | latest `rustup-init`, no checksum (S 29, 98-104) | `~/.cargo/bin` (D) | `~/.profile`, each bash rc, first of `$ZDOTDIR/.zshenv` or `~/.zshenv` (rustup source) | `--no-modify-path` | rustup-init prompt on `/dev/tty`; `-y` skips it (S 137-203) | `rustup self uninstall` | `rustup self update`; `auto-self-update` setting (D) |

What this Mac adds (M): `/usr/local/bin` is not writable by the user. `/opt/homebrew` is user-writable and owned by Homebrew. Claude Code and opencode are already script installs here; uv is a Homebrew install; rustup is reached through mise links into `~/.cargo/bin`. `~/.zshenv` sets `ZDOTDIR=~/.config/zsh`, so zsh never reads `~/.zshrc`, and boat's `boat()` function would never load.

Not determined: boat's version command, where boat stores its account token, and where boat's updater writes; whether `claude install` prompts. Scripts and their URLs change without notice, so the hashes above are evidence for this date only.

## Goals

- **G1.** `dfm trial <tool> --script <url>` installs a tool through its script with the same approval, record-first and receipt rules as a provider trial (docs/trial.md).
- **G2.** The run never writes a tracked dotfiles file, a dfm-owned link, or the manager's own state. This is enforced while the script runs and verified after it.
- **G3.** Every effect inside a fixed watch set is recorded, created or changed. Every effect outside it is either impossible (denied) or listed as `unknown`.
- **G4.** The bytes that run are the bytes the owner approved.
- **G5.** `adopt` and `purge` work on a script trial with the same safety rules they have today.
- **G6.** dfm states who owns each script tool's upgrades and reports a changed binary and a second copy on PATH.

## Non-goals

- Pinning or verifying what the script downloads. dfm cannot pin a download that the script chooses.
- Restoring a file that the script changed. Purge has no restore today ("No restore", docs/purge.md), and this design adds none.
- A general OS sandbox for any command. The sandbox below is for the script run only.
- Running scripts during `sync` for upgrades. A script tool is never re-installed to update it.
- Parsing or rewriting scripts. dfm treats the script as opaque bytes, except for the two text hints in the preview.

## Anti-goals

- A trial that leaves a change in `~/.dotfiles` for the owner to find later.
- A trial that approves one script and runs other bytes.
- A run that waits forever on a prompt that nobody sees.
- Attribution claims dfm cannot support. A path that changed during the run is "observed during the script run", not "written by the script".

## The command

```
dfm trial <tool> --script <url> [--shell sh|bash|zsh] [--arg <value>]... [--env NAME=VALUE]... [--tty] [--id <trial-id>] [--config <path>]... [--preview] [-y|--yes-plan <digest>]
```

- `<tool>` is the short name (`boat`), with the existing id rule. `--script` replaces the provider; it cannot be combined with `--provider`.
- An executable named `<tool>` already on PATH refuses the trial (`E_CONFLICT`, naming each path): a second install over it would mix two owners in one record. Remove or purge the existing copy first.
- `--arg` values go to the script after its path (`sh <script> --no-modify-path`). `--env` values are added to the script's environment. Both are in the digest and the record. No check refuses a secret here: the store's secret check matches only values that the test harness supplies (docs/state.md, rule 11). The record is a private state file. Adopt scans everything it writes into the repository with its existing `gitleaks` scan, the declaration included.
- "On PATH" means the PATH of the dfm process (the owner's shell), as `doctor` reads it, plus the fixed provider directories that the other-owner check uses (`crates/cli/src/adapters/package.rs`).

### The script's environment

The script does not get dfm's environment, which can hold secrets (for example the exports of `~/.config/zsh/.personal.env`). It gets the runner's cleared environment (`crates/engine/src/proc.rs`) plus a fixed list, so that it chooses the same files as a `curl | sh` in the owner's shell:

- `HOME`, `USER`, `LOGNAME`, `TMPDIR`, and `SHELL` from the account's login shell;
- `PATH`: the runner's fixed PATH;
- `LANG` and `LC_ALL` as dfm has them, and `TERM` with `--tty` only;
- `ZDOTDIR`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME` and `XDG_BIN_HOME`, each only when dfm's environment sets it.

These values are paths and names, not secrets. They go into the digest and the record. `--env` cannot set any of them (usage error), because they move the script's writes, and the watch set and the sandbox are computed from them.

## Identification and approval (C04)

1. **Fetch once.** The preview fetches the URL with `/usr/bin/curl --proto =https --proto-redir =https --tlsv1.2 -fsSL --max-filesize 1048576`. Only `https` is accepted, also after redirects. The bytes are stored in the manager's cache as `scripts/<sha256>` (mode 0600), and the preview prints that path so the owner can read the script before approving.
2. **The digest.** The trial digest (docs/trial.md, step 3) gains: the URL, the effective URL after redirects, the byte count and SHA-256, the shell, the arguments, the environment, and `tty`. The configuration and data candidates stay in the digest as today. The watch set (below) is not in the digest: HOME changes all the time, and the digest would always be stale.
3. **Run the approved bytes.** Under the lock, dfm runs the cached file whose SHA-256 is the approved one: `<shell> <cache>/scripts/<sha256> <args>`. It never fetches the URL again. A missing or changed cache file is `E_PLAN_STALE`.
4. **dfm runs the script.** The owner does not run it by hand under observation. Only dfm can hold the writer lock, write the record first, apply the sandbox and observe before and after in one run.
5. **Downloads stay unknown.** The digest covers the script, not the code the script fetches. The preview prints `unknown: script-downloads`. After the run, the record keeps the SHA-256 of each new regular file in a watched bin directory, as evidence of what arrived.
6. **No grant for a trial.** The approved digest is explicit trust in those exact bytes. A grant is needed only where no digest is approved, in `sync` (see "Adopt").

Preview hints, from plain text matches in the script: when it contains `/dev/tty`, `note: the script reads /dev/tty; pass --tty to answer its prompts`. When it names `.zshrc`, `.zshenv`, `.bashrc` or `.profile`, `note: the script edits shell startup files; files in the dotfiles repo are write-protected during the run; pass the script's own opt-out if it has one`. A hint is not a guarantee in either direction.

## The run and the sandbox (C03)

The script runs under `/usr/bin/sandbox-exec` with a profile that dfm generates for the run:

- **Allow** by default, then **deny** `file-write*` outside HOME, the user's temporary directory (`getconf DARWIN_USER_TEMP_DIR`), `/private/tmp` and `/dev`.
- **Deny** `file-write*` below each guarded checkout: the bound checkout and each nested source checkout that the published lock declares (the set of `guarded_checkouts`, `crates/cli/src/sync_cmd.rs`).
- **Deny** `file-write*` on each target path that the ledger records (the dfm links themselves), so a script can neither write through a link nor replace or remove it.
- **Deny** `file-write*` on the manager's reserved paths (the `reserved` set in `crates/engine/src/store.rs`): below the state root, the configuration directory that holds the machine binding, the data directory and the cache directory, and on the writer lock file itself. The lock file rule also denies its removal, so a second writer cannot take a new lock while the trial holds the old one. Also deny `file-write*` below the manager's installation directory.
- **Deny** `process-exec` of `/usr/bin/sudo`, `/usr/bin/su` and `/usr/bin/osascript`, so the script cannot ask for administrator rights.

Tested on the M5 (Darwin 27.2.0, 2026-11-03, scratchpad only): an append and a truncate through a link into a denied directory fail with "Operation not permitted"; `mv` over the link and `rm` of the link fail; a child process inherits the denial; `sudo` fails with exit 126; writes in HOME and in the temporary directory succeed; a write to `/private/var/tmp` fails.

A denied write fails inside the script. The script then continues or stops on its own terms, and its output tells the owner which write failed. The trial still records what happened (see "Record").

After the script exits, the tracked-file guard of DFM-25 runs as a second check: the `tracked_status` snapshot before and after. A tracked change, which the sandbox should make impossible, makes the trial exit 1 with `E_CONFLICT` naming each file. The change is left in place, not reverted, staged or committed, as for an updater (docs/upgrade.md, "Tracked files").

`sandbox-exec` is deprecated by Apple but present and working on this macOS. When it is missing or cannot apply the profile, a script trial is `E_PLATFORM_UNSUPPORTED` (exit 3, nothing written). There is no fallback without the sandbox.

## The observation window (C02)

dfm observes the watch set under the lock, just before the script starts and again after it exits.

| Watched | Observation |
| --- | --- |
| Shell startup files: `~/.profile`, `~/.bash_profile`, `~/.bash_login`, `~/.bashrc`, `~/.zshenv`, `~/.zprofile`, `~/.zshrc`, `~/.zlogin`, `~/.zlogout`; the same zsh names under `$ZDOTDIR` when the environment sets it; `~/.config/fish/config.fish` | kind, link target, size and SHA-256 of the content (through a link) |
| Directories `~/.config/fish/conf.d`, `~/.local/bin`, `~/bin`, `/usr/local/bin`, `/opt/homebrew/bin` | each entry: name, kind, link target, SHA-256 of a regular file up to 16 MiB |
| Top-level listings of HOME, `~/.config`, `~/.local`, `~/.local/share`, `~/.local/state`, `~/.cache`, `~/Library/Application Support`, `~/Library/LaunchAgents` | each entry: name and kind |
| The configuration and data candidates of docs/trial.md | as today |

- A difference is `created`, `modified`, `removed` or `kind-changed`. Each goes in the record with its kind and hash or link target.
- Writes outside HOME, except the temporary directories, are denied, so they cannot happen. `/usr/local/bin` and `/opt/homebrew/bin` are watched to prove this, and the denial keeps a script out of Homebrew's prefix.
- `~/.local/bin` is in HOME and allowed; its new entries are recorded.
- Every process on the machine writes during the window, not only the script. The record says "observed during the script run". Purge needs the owner's approval for each path (as for `existed: false` today).
- **Stays `unknown`:** a new file inside a directory that existed before and is not watched (for example a new file in an existing `~/.config/foo/`); writes in the temporary directories; effects of the tool's later self-updates; launchd jobs registered without a plist in `~/Library/LaunchAgents`; keychain items; remote effects of onboarding (an account or a token on the vendor's side); the content the script downloaded (`script-downloads`).

## Interactive steps (C05)

- **Default: no terminal.** The script starts in a new session (`setsid`), with standard input from `/dev/null` and standard output and error to dfm's own. `/dev/tty` cannot be opened, so a prompt fails at once instead of waiting. The owner sees the script's output as it runs. It is not withheld, because the owner started this run to watch it.
- **`--tty`.** The script runs with dfm's terminal as standard input, so `/dev/tty` prompts work (boat's `boat onboard`, rustup without `-y`). It runs in its own process group, as a provider install does. dfm makes that group the terminal's foreground group (`tcsetpgrp`) for the run, and takes the terminal back after the script exits, with SIGTTOU blocked while it does so. The writer lock is held while the owner answers. The sandbox still denies `sudo`, `su` and `osascript`.
- A stop request ends the script's process group, as for a provider install: SIGTERM, the grace period, then SIGKILL (`crates/engine/src/proc.rs`). dfm is not in that group, so it stays alive and writes the record and the window observed so far. With `--tty`, a terminal SIGINT goes to the script's foreground group and not to dfm. dfm records the run when the script exits.

For boat, this means `dfm trial boat --script https://boat.dev/install --tty`. Without `--tty`, `boat onboard` reads `/dev/null`, and the installer exits with onboard's status.

## Record

The trial record gains a `script` object and becomes schema 5 (the docs/state.md rule for a new document field: a binary built before it reports `E_STATE_NEWER`). `package` is left out.

- `script.url`, `effective_url`, `sha256`, `bytes`, `shell`, `args`, `env` (the fixed list and `--env`), `tty`
- `script.before`: the watch-set observation taken just before the script starts
- `script.exit`: the script's exit code, or the signal
- `script.window`: one entry per difference: `path`, `change`, `kind`, `sha256` or `target`, and `before` for a modified file
- `script.binary`: the tool's identity after the run: the first executable named `<tool>` on PATH, as `path` (the PATH entry, for example `~/.local/bin/boat`), `real_path` (through links, for example `~/.ascii/bin/boat`) and the SHA-256 of the real file. Left out, with the note `the tool is not on PATH`, when none is found. Other executables the script added (`uvx`, `cargo`) are window entries only.
- `unknown`: the existing tokens plus `script-downloads`, `later-self-updates`, `keychain` and `remote-effects`

Order of one run: the record is created with everything except `exit`, `window` and `binary`, and with `script.before`, before the script starts. A `trial-script` journal intent follows. After the script exits, the store rewrites the record once with the result (as `mark_trial_adopted` rewrites it today), then the `done` or `failed` entry and the receipt follow.

A crash, a SIGKILL or a sleep that ends the run leaves a record with `before` and no `window`. `state check` reports the open intent (`E_OP_INTERRUPTED`), as for an interrupted install. `purge` then decides from `before` against the live state: every entry created since `before` is a candidate, each with the owner's approval. A script trial whose record has no `window` cannot be adopted.

Exit codes follow docs/trial.md. A script that exits nonzero is exit 1 after the record: the window still shows what it did, and purge can remove it.

## Adopt (C06)

- **Configuration candidates:** the record's `baseline.paths` as today, plus each window entry `created` at the top level of `~/.config` (boat's `~/.config/ascii`). Other window entries (data, caches, bin directories, the installation directory) are never promoted.
- **Install declaration:** a script tool has no Brewfile line. Adopt appends a `script/v1` package declaration to the module: the URL, the effective URL seen by the trial, the shell, the arguments, the `--env` values and `script.binary.path`. A record without `script.binary` cannot be adopted (`E_CONFLICT`, "the tool is not on PATH"). The format is left to the builder.
- **Declaration only.** A script tool can be adopted with no configuration candidate (uv and rustup create no `~/.config` entry). Adopt then writes the declaration only. Today's "nothing to adopt" refusal applies to a script trial only when it has neither a candidate nor a binary.
- **`sync` with a declaration:** the packages scope reports the declaration `current` when the binary path exists and is executable. It never runs the script again for a present binary. When the binary is missing (a new machine), the scope installs it with the same sandbox, environment rule and guard, without a terminal, and only when the URL has a grant `script:<url>` in `state/grants.json` (a third list beside `origins` and `packages`, written by `dfm sync --trust-origin 'script:<url>'`). Without the grant the row is `blocked` with that command. A redirect is followed only when it ends at the declared effective URL; any other target is `blocked` with both URLs. A script that needs a prompt fails its row. The next sync finds the binary and reports it `current`.

## Purge (C06)

- **Removed, with approval per path:** each window entry `created` in any watched listing that is still present, whatever its content is now. This is the existing rule for a path that was absent before the trial (docs/purge.md), and the purge digest covers the live fingerprint. It includes a new directory (boat's `~/.ascii`, Claude Code's `~/.local/share/claude`) with its tree, under the existing tree rules; a new regular file (a bin file that `uv self update` replaced, uv's new `~/.profile`, a fish `conf.d` file); and a new link in a watched bin directory.
- **A created link** is removed with `unlink` and never followed, when its target is the recorded target or is inside a directory that the trial created (Claude Code's link after `claude update` points to a new `versions/<ver>` inside `~/.local/share/claude`). Any other created link is kept: the owner repointed it. This is a narrow exception to "a link is kept".
- **Kept and printed as `retained:`:** each `modified` file, with its hashes before the run and now and a hint (`edit ~/.profile by hand`); each `removed` or `kind-changed` entry; a created plist in `~/Library/LaunchAgents`, with the hint to unload it with `launchctl bootout` and remove it by hand (services stay `unknown`, as in docs/trial.md); every `unknown` effect. Under the sandbox, no `modified` entry can be inside `~/.dotfiles` or a dfm link.
- The trial record is removed last, as today.

## Upgrades and reporting (C07)

- **The tool owns its upgrades**, for a trial and for an adopted script tool. dfm does not run the script again to update, because a re-run repeats the startup edits and onboarding (uv's own docs say `uv self update` can modify shell profiles). dfm does not change the tool's self-update setting. That stays the owner's choice (`BOAT_NO_UPDATE`, `DISABLE_UPDATES`, opencode's `update`).
- **A dfm updater** for a script tool exists only when a manager release adds it to the fixed set, as for `claude`, `codex` and `opencode` (docs/upgrade.md). A document cannot add one.
- **dfm never runs the tool binary to read a version.** boat checks for updates after any command ends, so `boat --version` could start an update.
- **`doctor`** gets one row per active script trial and per adopted script declaration:
  - OK with a note when the binary changed since the trial (`binary changed since trial: sha256 a… -> b…`). A self-update is expected. Each `doctor` run resolves `script.binary.path` (or the declared binary path) through its links again and hashes the file that it points to now. It does not hash the recorded `real_path`, because an updater can point the link to a new file and leave the old one in place (Claude Code points `~/.local/bin/claude` to a new `versions/<ver>` file). The note names both real paths when they differ.
  - WARN when the binary path or its link target is missing.
  - WARN when PATH holds more than one executable named `<tool>` that resolves to different files, listing each path and its owner (`homebrew/v1`, `mise/v1`, `script trial <id>`, or `unknown`).
- The Homebrew second-copy note for `claude`, `codex` and `opencode` stays as it is.

## Decided in this design (my calls, open to the owner)

1. **Sandbox required.** A script trial needs `sandbox-exec`. Without it there is no script trial. The alternative is detection only (the tracked-file guard), which reports a repo edit after it happened.
2. **Writes outside HOME are denied**, except the temporary directories. A script that must write elsewhere fails. This also keeps scripts out of `/opt/homebrew`.
3. **No terminal by default**, `--tty` to attach one. The alternative is a terminal by default, as when the owner runs `curl | sh` by hand.
4. **dfm runs the script**, not the owner under observation.
5. **A pre-existing tool refuses the trial**, unlike provider trials, which record a pre-existing tool.
6. **Adopt writes a `script/v1` declaration that `sync` installs on a new machine** after a `script:<url>` grant, and a script tool can be adopted with the declaration only. Owner decision on 2026-11-03. The owner declined the smaller alternative, in which adopt only prints the install command for a new machine.
7. **Purge removes links it can attribute** (created in a watched bin directory, target unchanged), as an exception to the current link rule.
8. **The tool owns its upgrades**; dfm reports and does not update.
9. **The script's output is shown**, not withheld. Command output elsewhere is withheld (docs/secrets.md), but the owner runs a script trial to watch it.
10. **The script gets a cleared environment** with a fixed list of path variables, not the owner's environment, which can hold secrets. A tool whose installer needs another variable gets it through `--env`.
11. **No secret check on `--env` and `--arg`** in the trial record, which is private. Adopt's `gitleaks` scan covers the declaration before it reaches the repository.

## Left to the builder

- How the cache stores fetched scripts and when it removes them.
- The profile text and how dfm escapes paths in it. A path with a character the profile language cannot hold refuses the run.
- The `script/v1` declaration format and where its resolver rule lives.
- Limits: entries per watched directory, total window entries, script size (1 MiB above).
- The doctor code name.
- How the sandbox tests run where `sandbox-exec` is missing (for example a Linux CI runner): they skip with a note, as the real-gitleaks tests do.
- Tests, with look-alike scripts in the test-isolation build: a write through a link is denied and the repo file is unchanged; a replaced or removed link is denied; `sudo` is denied; a write outside HOME is denied; a write to the machine binding and a removal of the lock file are denied; a rename of an ancestor of a protected path (for example `~/.config`) is denied; with `--tty`, a stop request ends the script and dfm still writes the window; `doctor` sees an update that points the binary link to a new file; a created directory, bin file and link are recorded and purged; a modified startup file is retained; a changed cache file is `E_PLAN_STALE`; a redirect to `http` is refused; `--tty` off gives a failing `/dev/tty` read; a pre-existing tool is refused; adopt promotes a created `~/.config` entry and writes the declaration; sync installs a missing declared tool only with the grant.

## Delivery

Four slices, each its own PR:

1. `dfm trial --script`: fetch, digest, sandbox, window, record, guard.
2. `purge` of a script trial.
3. `adopt` of a script trial, the `script/v1` declaration, `script:` grants and the `sync` install.
4. The `doctor` row.
