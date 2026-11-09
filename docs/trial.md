# Trial

The `trial` command (`try <tool>` in DESIGN.md section 7): try a tool without authoring a module. This document describes the behavior at HEAD. The package providers it reuses are in [docs/packages.md](packages.md), the state store in [docs/state.md](state.md). `adopt` (DFM-33) is in [docs/adopt.md](adopt.md), `purge` (DFM-34) in [docs/purge.md](purge.md). The last section lists what they inherit.

| Part | Location |
| --- | --- |
| `trial` command: arguments, owner discovery, baseline, digest, install, receipt | `crates/cli/src/trial_cmd.rs` |
| Trial record fields (`TrialRecord`, `TrialPackage`, `TrialInstall`) | `crates/model/src/state.rs` |
| Record write (atomic, exclusive create, identity, claim and secret checks) and load | `crates/engine/src/store.rs` (`write_trial`, `load`) |
| Provider probes and installs (`homebrew/v1`, `mise/v1`) | `crates/cli/src/adapters/package.rs` |
| Tests with `brew` and `mise` look-alikes | `crates/cli/tests/trial.rs`, scripts in `crates/cli/tests/common/providers.rs` |
| Script trial: fetch, cache, environment, sandbox profile, run, record | `crates/cli/src/trial_cmd/script.rs` |
| Script trial window: watch set, caps, diff, installed binary | `crates/cli/src/trial_cmd/window.rs` |
| Script record types (`TrialScript`, `ScriptExit`, `WindowEntry`), schema 5 | `crates/model/src/state.rs` |
| Script trial run under the real `sandbox-exec` | `crates/cli/tests/trial_script.rs` |

## The command

```
dfm trial <package> [--id <trial-id>] [--provider homebrew/v1|mise/v1] [--config <path>]... [--preview] [-y|--yes-plan <digest>]
dfm trial <tool> --script <url> [--shell sh|bash|zsh] [--arg <value>]... [--env NAME=VALUE]... [--tty] [--id <trial-id>] [--config <path>]... [--preview] [-y|--yes-plan <digest>]
```

The second form installs a tool through a downloaded script. It is described in "Script trials". The rest of this document describes the first form unless it says otherwise.

- `<package>` is the package as the provider names it (`atuin`, `homebrew/core/atuin`, `node@20`), checked by the package adapter's name rule.
- The tool is its short name (`atuin`, `node`). It must be a valid id: lowercase ASCII letters, digits, `.`, `_`, `-`. A mise backend name such as `npm:prettier` is a usage error.
- The trial id is `trial-<tool>` unless `--id` names another.
- `trial` takes no `--root`: it reads no source tree. The machine must be bound.

Order of one run:

1. **Owner discovery.** The signal handlers go in. Then the providers are probed read-only as `packages` probes them (docs/packages.md). With `--provider`, only that provider is asked. Without it, both are asked:
   - A provider lists the package, or an executable of its name has another owner: the tool **pre-exists**. Its owners are recorded (`homebrew/v1 (18.24.1)`, or the other-owner text with its path), and nothing is installed.
   - Exactly one provider reports it missing and installable: that provider installs it, with the dependencies the provider names.
   - More than one can install it: the choice is the owner's. `E_APPROVAL_REQUIRED` (exit 3) names the candidates, with `--provider` as the fix.
   - None can: a blocker with the probe's own code (`E_PROBE_FAILED` when any provider could not answer, else `E_SOURCE_UNTRUSTED`, `E_PLATFORM_UNSUPPORTED`, `E_CONFLICT` or `E_PRIVILEGE_UNSUPPORTED`) and each provider's answer.
2. **Baseline.** The configuration candidates are the `--config` paths (absolute, or relative to HOME), or `~/.config/<tool>` when none is given. Each must pass the store's claim check (inside HOME, outside the manager's own paths) before any provider runs. Each is observed once. A regular file is hashed (`file_fingerprint`). A directory and an absent path are recorded as such. The **data candidates** `~/.local/share/<tool>`, `~/.local/state/<tool>`, `~/.cache/<tool>`, `~/Library/Application Support/<tool>` and `~/Library/Caches/<tool>` are observed the same way (a data candidate that is also a configuration candidate is observed once, as configuration). A data candidate that fails the claim check or the observation is left out of the record and printed as `data: <path> not observed (<reason>)`. It does not block the trial, and purge never removes it. A link, a special file, an unreadable entry or a file over 16 MiB is a **baseline failure**: `E_STATE_INVALID`, exit 3, nothing written. Recording such a path as absent would let a later purge delete what was there.
3. **Preview.** The decision prints: the trial and tool, the provider and package, the action (install with its dependencies, or nothing because the tool pre-exists and who owns it), one `baseline:` line per configuration candidate and one `data:` line per data candidate, the cleanup scope, the `unknown:` effects and `plan digest:`. `--preview` stops here, exit 0 (3 when blocked). Without `--yes-plan`, the run stops with `E_APPROVAL_REQUIRED` (exit 3) and the digest to pass. The digest is the SHA-256 of the canonical JSON of the decision: trial id, tool, adapter, package, the install and its dependencies or the pre-existing owners, every baseline and data path with `existed` and `sha256`, the unknown tokens, the binary identity and the binding stamp. A changed provider answer, candidate, binary or binding changes it.
4. **Under the lock.** The writer lock is taken. The identity and the ledger are checked, the providers are probed again and the baseline is observed again. A digest that differs now is `E_PLAN_STALE` (exit 3, nothing written). A trial record of the same id in `state/trials/` is `E_CONFLICT` (exit 3). This is checked before the lock and again by the exclusive create, so two concurrent runs cannot replace each other's record.
5. **The record first.** The trial record is created (`RENAME_EXCL`, never replacing) before the provider changes anything. Its `package.install.op_id` is the journal operation id of the install, so the record, the journal and the receipt share one id.
6. **The install.** For a missing tool: the `intent` journal entry (no path), the provider's install (`brew install homebrew/core/<name>`, `brew install --cask <name>`, `mise install <tool>@latest` or `mise install <tool>@<version>`, docs/packages.md), a probe as the postcondition (`Installed`, else `failed`), and the `done` or `failed` entry. An outcome entry that cannot be written (for example, a journal at its cap) makes the operation `failed` with both causes in its error. The receipt and the result line still follow, and the exit is 1, because the record was already written. A pre-existing tool journals nothing.
7. **The receipt.** `command = trial`, `op_id` as above, `plan_digest` and a `yes_plan` approval, and one operation (`updated`, `current` or `failed`) whose `id` is the journal id. The output ends with `trial: state/trials/<id>.json`, `receipt: <name>` and `result:`.

| Exit | When |
| --- | --- |
| 0 | The trial is recorded (installed, or pre-existing). |
| 1 | The install failed after the record was written. The record and the `failed` entry stay. |
| 2 | Usage. |
| 3 | Blocked before any write: approval missing, ambiguous provider, no capable provider, baseline failure, stale digest, existing trial, lock held, not bound. |
| 130, 143 | A stop request. The provider child is ended (docs/secrets.md, "How `op` is run"). |

A script trial uses the same codes. "Script trials" lists what each means there.

## The record

`state/trials/<trial-id>.json`, canonical JSON. Schema 2, or 3 once adopted. A record with data paths is schema 4, adopted or not. A record with `script` is schema 5. The docs/state.md validation applies: identity stamp, valid id, claim check on every path, secrets refused.

| Field | Meaning |
| --- | --- |
| `trial_id`, `tool`, `binding`, `created_unix_ns` | `tool` is the short name. |
| `baseline.tool_preexisting` | True when a provider listed the tool or another owner had it. |
| `baseline.owners` | The owners found when it pre-existed: `<adapter> (<version>)` per listing provider, or the other-owner text. Left out when empty. |
| `baseline.paths[{path, existed, sha256}]` | The candidates observed before the install. `sha256` is present for a regular file only. |
| `baseline.data[...]` | The data candidates, in the same shape. `adopt` never promotes them; `purge` removes the ones that were absent. Left out when empty, so a record written before they were observed still reads. |
| `owned_outputs` | Empty. The trial writes no configuration. |
| `package.adapter`, `package.name` | The provider that lists or installed the tool, and the package as it names it. Left out when only another owner has the tool. The journal and receipt then name the adapter `other-owner`. |
| `package.install{op_id, dependencies}` | Present only when the trial installed: the journal and receipt id, and the dependencies the provider added. |
| `unknown` | Fixed tokens `services`, `paths-outside-baseline`, `provider-caches`: effects the trial does not instrument. Candidates for review, never owned cleanup scope. |
| `script{...}` | Present only for a script trial. See "Script trials". |
| `adopted{op_id, module, at_unix_ns}` | Set once by `adopt` (DFM-33), with `schema` 3. It names the journal operation and the module that took the configuration. Left out of an active trial. |

The optional fields `package` and `unknown` need no schema bump: a record without them (for example, one the test harness `__record-trial` writes) still reads. `adopted` is different: the record type refuses unknown keys, so an adopted record is schema 3 and an unadopted record stays schema 2. The validator accepts 2 and 3, refuses `adopted` on schema 2, and requires it on schema 3. A binary that predates schema 3 reports an adopted record as `E_STATE_NEWER`. `baseline.data` follows the same rule: a record that carries it is schema 4, with or without `adopted`, and `adopt` keeps schema 4. The validator refuses data on schema 2 or 3. A binary that predates schema 4 reports such a record as `E_STATE_NEWER`. `script` follows the same rule: a record that carries it is schema 5, with or without `baseline.data` and `adopted`. The validator requires schema 5 when `script` is present and refuses it on any other schema. A binary that predates schema 5 reports the record as `E_STATE_NEWER`.

A trial is **active** until its record has `adopted`. An adopted record stays as evidence and keeps its id taken (a second trial of that id is `E_CONFLICT`). Readers that mean "in progress" skip it: `doctor` lists no row for it, and `packages --update` does not roll its tool from the record. `state check` still validates and counts it.

The record is a private file under `state/trials/`. `state check` validates it with the other documents and counts it (`trials: N (M unreadable)`). `doctor` and `status` show an unreadable or foreign-identity record as a WARN row. An interrupted install follows the packages rule (docs/packages.md, "Interrupted install"): `state check` reports `E_OP_INTERRUPTED`, `state recover` leaves it `unknown`, and `state resolve <op_id> --as failed` settles it.

## Script trials

`dfm trial <tool> --script <url>` runs an install script that dfm downloads, under a macOS sandbox, and records what changed. It exists for tools that install only through `curl -fsSL <url> | sh`. The design is `docs/superpowers/specs/2026-11-03-dfm-78-script-trials-design.md`. Where the code differs from it, this section describes the code and "Differences from the design" names the difference. `purge` and `adopt` of a script trial are not built (DFM-82, DFM-83).

### Arguments

| Argument | Meaning |
| --- | --- |
| `<tool>` | The short name. It must be a valid id (see above). |
| `--script <url>` | An `https://` URL. It replaces the provider, so `--provider` is a usage error. |
| `--shell <shell>` | `sh`, `bash` or `zsh`. The interpreter. It runs as `/bin/<shell>`. Default `sh`. Any other value is a usage error. |
| `--arg <value>` | A script argument, passed after the script path. Repeatable, kept in order. A control character in the value, a tab included, is a usage error. |
| `--env NAME=VALUE` | An added environment variable. Repeatable. The name must match `[A-Za-z_][A-Za-z0-9_]*`, be unique and not be one of the fixed names below. A control character other than a tab in the value is a usage error. |
| `--tty` | Gives the script dfm's terminal as standard input, so a prompt on `/dev/tty` works. |
| `--shell`, `--arg`, `--env`, `--tty` | Each needs `--script`. |

Without `--tty` the script starts in a new session with standard input from `/dev/null`. A read of `/dev/tty` fails at once. With `--tty`, Ctrl-Z is ignored by the script's group: dfm does not wait for a stopped script, so a stop would block the terminal until the timeout. Standard output and standard error go to dfm's own, so the owner sees the script's output as it runs.

### Order of one run

1. **Pre-existing tool.** An executable named `<tool>` in dfm's `PATH` or in the provider directories is `E_CONFLICT` (exit 3), naming each path. A second install over it would mix two owners in one record. Nothing is fetched.
2. **Sandbox check.** `sandbox-exec` is resolved with `/usr/bin` first on the search path. When it is missing, the run is `E_PLATFORM_UNSUPPORTED` (exit 3). Then the profile for this run is applied once to `/usr/bin/true`. A nonzero exit is also `E_PLATFORM_UNSUPPORTED`. Nothing is written or cached when this check refuses.
3. **Fetch.** A preview (no `--yes-plan`) downloads the script and caches it. With `--yes-plan`, the run uses the cached bytes and never downloads again. A missing cache file, a missing sidecar or a changed file is `E_PLAN_STALE`.
4. **Preview.** The lines are: `trial:`, `script:` (URL, effective URL, SHA-256, bytes), `script file:`, `run:`, `env:`, the notes below, the `baseline:` and `data:` lines, `watch:`, `cleanup scope:`, `unknown:` and `plan digest:`. The `run:` line names the cache file; the approved run executes the run copy of step 8, which holds the same verified bytes. `--preview` stops here, exit 0. Without `--yes-plan`, the run stops with `E_APPROVAL_REQUIRED` (exit 3) and the digest to pass.
5. **Terminal check.** With `--tty`, standard input must be a terminal. This is checked only when the run would start, so a preview works without one. Otherwise it is a usage error.
6. **Under the lock.** The writer lock is taken. The identity and the ledger are read again, the cached script is read again, and the environment, the profile and the baseline are rebuilt. A digest that differs is `E_PLAN_STALE` (exit 3, nothing written). The pre-existing tool check of step 1 runs again, because another installer can add the tool in the meantime (`E_CONFLICT`, exit 3, nothing written). A trial record of the same id is `E_CONFLICT`. This is checked before the lock and again by the exclusive create in step 8, after the window observation.
7. **Window before.** The tracked-file snapshot of the guarded checkouts and the window observation are taken. Both come before the record, so the record never claims a window that was not observed.
8. **The record first.** The verified bytes are copied to a private run copy, `<cache dir>/scripts/run-<id>-<pid>` (mode 0600, exclusive create; an existing entry is `E_CONFLICT`). The run copy is the only write before the record, and every exit path after it removes it. Then the record is created (`RENAME_EXCL`) with `script.before` and without `exit`, `window` and `binary`.
9. **The intent.** A `trial-script` journal intent (adapter `script/v1`, no path) is written.
10. **The run.** `sandbox-exec -p <profile> /bin/<shell> <run copy> <args>` runs with the environment below and a fixed timeout of one hour (no flag yet). The script leads its own process group. Its working directory is a new private directory, `<TMPDIR>/dfm-trial-<id>-<pid>` (mode 0700; an existing entry is `E_CONFLICT` and the script does not run), which the run removes afterwards, so an installer that writes to `.` works.
11. **The result.** The window is observed again and compared. The tracked-file snapshot is compared. The record is rewritten once with `exit`, `window` and `binary`. The cache files and the run copy are removed. The journal gets `done` or `failed`.
12. **The receipt.** The receipt file is written before any of the closing output lines. The output ends with `trial: state/trials/<id>.json`, the `note:` about a missing binary when it applies, `receipt: <name>` and `result:`.

The preview notes are plain text matches on the script and are not guarantees. `note: the script reads /dev/tty; pass --tty to answer its prompts` appears when the text `/dev/tty` occurs. `note: the script edits shell startup files; ...` appears when `.zshrc`, `.zshenv`, `.bashrc` or `.profile` occurs.

The digest is the SHA-256 of the canonical JSON of: trial id, tool, the script (URL, effective URL, SHA-256, bytes, shell, arguments, environment, `tty`), every baseline and data path, the skipped data candidates, the unknown tokens, `profile_sha256`, the binary identity and the binding stamp. The window watch set is not in the digest.

### Fetch and cache

| Item | Value |
| --- | --- |
| Script file | `<cache dir>/scripts/<sha256 of the URL>`, mode 0600 |
| Sidecar | `<same name>.json` with `url`, `effective_url`, `sha256` and `bytes`, mode 0600 |
| Directory | `<cache dir>/scripts/`, created with mode 0700 |
| Size cap | 1 MiB (1 048 576 bytes) |

The cache dir is `~/.cache/dotfiles-manager`. A preview leaves its files in place until a run. A stale `.fetch-<pid>` temporary file is removed before curl runs, because curl follows a link at its `-o` path. A preview takes no lock and can replace the script file while a run waits, so the run executes its own copy, never the script file. A run that created the record removes the script file, the sidecar and the run copy, also when the rewrite failed or the script never started. The record keeps the script's SHA-256, so a later preview downloads the script again.

curl runs through the runner, with `/usr/bin` moved to the front of the search path so that the system's curl runs even when a provider directory comes first. Its arguments are `-q` first, then `--proto =https --proto-redir =https --tlsv1.2 -fsSL --max-filesize 1048576 -o <temporary file> -w %{url_effective} -- <url>`, with a 120 second timeout. `-q` stops `~/.curlrc` from weakening the TLS options.

| Result | Code |
| --- | --- |
| curl exit 63 (`--max-filesize`), or a body over the cap | `E_LIMIT_EXCEEDED` |
| Any other curl failure, a timeout, or an effective URL that is not `https://` | `E_SOURCE_UNTRUSTED` |

Both refuse before any write to the state store. The temporary file is removed.

### The script's environment

The script does not get dfm's environment. It gets exactly these variables, sorted by name:

| Variable | Value |
| --- | --- |
| `HOME` | The bound HOME. |
| `USER` | `USER` from dfm's environment, else `LOGNAME`, else the output of `id -un`. |
| `LOGNAME` | `LOGNAME` from dfm's environment, else the `USER` value. |
| `TMPDIR` | dfm's `TMPDIR` when it is absolute, else `getconf DARWIN_USER_TEMP_DIR`. Canonicalized, because the sandbox matches real paths. |
| `SHELL` | dfm's `SHELL` when it is absolute, else `/bin/sh`. The login shell is not read from the directory service. |
| `PATH` | The runner's fixed path. |
| `LANG`, `LC_ALL`, `ZDOTDIR`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_BIN_HOME` | Copied from dfm's environment when set and not empty. |
| `TERM` | Copied with `--tty` only. |
| `--env` pairs | As given. |

`id` and `getconf` run through the runner with `/usr/bin` first. The `--env` pairs cannot set any name in this table. The values go into the digest and the record. They are not checked for secrets: the record is a private state file.

### The sandbox

The profile is SBPL text with one rule per line. A later rule wins over an earlier one.

1. `(version 1)` and `(allow default)`.
2. Deny `file-write*` on the subpath `/`.
3. Allow `file-write*` on the subpaths of HOME, the canonical `TMPDIR`, `/private/tmp` and `/dev`.
4. Deny `file-write*` on each denied tree as a subpath: the guarded checkouts of the bound root (`guarded_checkouts`, `crates/cli/src/sync_cmd.rs`), the state root, the configuration directory, the data directory, the cache directory, and the real directory of dfm's executable (the parent of the resolved path).
5. Deny `file-write*` on each denied path as a literal, in both forms. The denied paths are every ledger target, the lock file and dfm's own executable path. The no-follow form names the link itself, so the script cannot rename, remove or replace it. The resolved form stops a write through the link. Each link strictly below HOME on the way to a denied tree or path is also a literal, and so is each ancestor of a denied tree or path that is strictly below HOME. A rename or `chmod` of `~/.config` is denied. A new entry inside `~/.config` is allowed.
6. Deny `process-exec` of `/usr/bin/sudo`, `/usr/bin/su` and `/usr/bin/osascript`.

A path that holds `"`, `\` or a control character cannot be written into the profile. The run is `E_TARGET_INVALID`. A non-UTF-8 path is refused the same way.

A hard link from HOME to a protected file is denied by rules 2 to 5, so there is no `file-link` rule. The exec deny has one purpose: the script cannot ask for administrator rights. It is not containment. A copied `osascript`, `open`, `launchctl` and Apple Events are not blocked, so services stay `unknown`.

The profile text is hashed. `profile_sha256` is in the digest, so a ledger change between the preview and the run is `E_PLAN_STALE`.

### The window

The window observes these places before and after the script. A path that is absent, and a directory that cannot be read, add nothing: the window is evidence, not authority.

| Place | Recorded for each entry |
| --- | --- |
| `~/.profile`, `~/.bash_profile`, `~/.bash_login`, `~/.bashrc`, `~/.zshenv`, `~/.zprofile`, `~/.zshrc`, `~/.zlogin`, `~/.zlogout`, `~/.config/fish/config.fish`, and the five zsh files under `ZDOTDIR` when it is set and is not HOME | Kind, SHA-256 of a regular file, and the link target of a link. |
| `~/.config/fish/conf.d`, `~/.local/bin`, `~/bin`, `/usr/local/bin`, `/opt/homebrew/bin` | Each child: kind, SHA-256 of a regular file, and the link target. For a link to a regular file, the SHA-256 is of the file behind it. |
| HOME, `~/.config`, `~/.local`, `~/.local/share`, `~/.local/state`, `~/.cache`, `~/Library/Application Support`, `~/Library/LaunchAgents` | Each child by name and kind only. A change inside a child is not seen. |

The first record of a path wins, so a file that a listing also names keeps its digest.

| Cap | Value | Over the cap |
| --- | --- | --- |
| Children of one watched directory | 10 000 | `E_LIMIT_EXCEEDED` |
| Observed entries in one pass | 15 000 | `E_LIMIT_EXCEEDED` |
| File size that is hashed | 16 MiB | The entry is recorded without a SHA-256. |

An entry with a control character in its name is recorded with kind `other` and a visible spelling of the name. Over a cap before the script starts, the run refuses before the record is written. Over a cap after the script, the record is written without `window` and the run exits 1.

The record also has a size budget of 3 MiB of serialized JSON, below the 4 MiB state write cap. The entry caps alone do not bound the size, because a long path makes every entry larger.

- Before the script starts, the record with `before` may use at most half of the budget (1.5 MiB). A larger record refuses with `E_LIMIT_EXCEEDED` (`the watch set is too large to record (<n> bytes, cap <cap>); nothing was written`) and exits 3. The script does not run and nothing is written.
- After the script, a record over the budget is shrunk until it fits. First the `before` of every window entry is removed, because `script.before` holds the same observation. If the record is still too large, window entries are cut from the end of the path order. `exit` and `binary` are always kept.
- A shrunk record has the token `window-truncated` in `unknown`, and the output has `note: the window was shrunk to fit the record: <k> entries dropped, before-states removed; the exit and the binary are recorded` after the `trial:` line when the record was written. `, before-states removed` appears only when the window had any, and `the exit is recorded` replaces the last part when `binary` is absent. The shrink alone does not fail the run. When the tool note also applies, the two share one `note:` line, separated by `; `.

`script.binary` is the first executable named `<tool>` in dfm's `PATH` and then in the provider directories: `path` as found, `real_path` through links, and the SHA-256 of the real file. It is absent when no executable is found or the real file cannot be hashed (over 16 MiB or unreadable). The output then has `note: the tool <tool> is not on PATH or cannot be hashed` after the `trial:` line, before `receipt:` and `result:`.

### Record fields

`script` in `state/trials/<id>.json` (schema 5):

| Field | Meaning |
| --- | --- |
| `url`, `effective_url` | The requested URL and the URL after redirects. Both `https://`. |
| `sha256`, `bytes` | The script as downloaded. |
| `shell`, `args`, `env`, `tty` | How it ran. `env` is a list of `[name, value]` pairs, sorted by name. |
| `before` | The window observation taken before the script started. |
| `exit{code, signal}` | Exactly one is set. `signal` is `stopped` (a stop request), `timeout` or `signal`. Absent until the run ends, and absent when the script could not be started. |
| `window[{path, change, kind, sha256, target, before}]` | One entry per difference. `change` is `created`, `modified`, `removed` or `kind-changed`. `before` describes a path observed earlier. Absent until the run ends. |
| `binary{path, real_path, sha256}` | The installed tool, as above. |

`unknown` holds the three package tokens and `script-downloads`, `later-self-updates`, `keychain` and `remote-effects`, and `window-truncated` when the window was shrunk. `package` is absent and `tool_preexisting` is false. `baseline.paths` and `baseline.data` are observed as for a package trial.

A crash, a SIGKILL or a system sleep that kills the run leaves a record with `before` and no `window`. When the intent was written, `state check` reports the open `trial-script` intent as `E_OP_INTERRUPTED`. The intent has no path, so `state recover` leaves it `unknown`, and only `dfm state resolve <op_id> --as failed` settles it (docs/packages.md, "Interrupted install"). A crash between the record create and the intent leaves the record and no intent, so `state check` reports nothing for it.

### Result and exits

| Exit | When |
| --- | --- |
| 0 | The script exited 0, the window was recorded and no tracked file changed. |
| 1 | After the record was written: the script exited nonzero, timed out or was ended by a signal; a tracked file changed (`E_CONFLICT`); the window could not be recorded; the record rewrite, the cache removal or the tracked-file check failed; or an outcome entry could not be written. The record stays. An `E_IO_FAILED` from the fetch or cache code before any state write also exits 1. |
| 2 | Usage. |
| 3 | Blocked before any write: no approval, stale digest or cache, a pre-existing tool, no usable sandbox, a failed fetch, a path the profile cannot hold (`E_TARGET_INVALID`), a failed `id` or `getconf` probe, a window cap reached before the run, an existing trial, lock held, not bound. |
| 130, 143 | A stop request. The script's process group gets SIGTERM and SIGCONT, then SIGKILL after the grace period or once the leader is reaped. A descendant can get less than the full grace period once the leader is reaped. The record, the window and the receipt are still written. |

When several problems occur in one run, the result carries the highest-ranked code and names every problem. The order is a stop request, then `E_CONFLICT` (a tracked file changed), then the rest. After the script, dfm compares the `tracked_status` snapshot of the guarded checkouts. A change is left in place, not reverted, staged or committed (docs/upgrade.md, "Tracked files"). The sandbox should make such a change impossible, so this is a second check.

The journal operation is `trial-script`. `doctor` shows an active script trial as `<tool> installed by the trial through <url>`. `purge` and `adopt` refuse a script trial with `E_CONFLICT` until DFM-82 and DFM-83.

### Differences from the design

- curl gets `-q` as its first argument. The design omitted it.
- curl, `id` and `getconf` run with `/usr/bin` first on the search path.
- The digest adds `profile_sha256`, so a ledger change between the preview and the run is `E_PLAN_STALE`.
- The sandbox check runs before the fetch. A system without a working sandbox caches nothing.
- Each denied path is denied as the link itself and as its resolved form, with every link strictly below HOME on the way.
- `SHELL` does not come from the account's login shell. It is dfm's `SHELL` when absolute, else `/bin/sh`.
- The timeout is a fixed hour with no flag.

### Tests

`crates/cli/tests/trial_script.rs` uses a `curl` look-alike and the real `/usr/bin/sandbox-exec`. No test reaches the network. The sandbox tests skip with a note where `/usr/bin/sandbox-exec` is missing or the user is root. The `--tty` stop test runs under `/usr/bin/script` and skips where it is missing or no terminal can be made. The unit tests in `script.rs` and `window.rs` need neither.

### Script trial limits

- **The sandbox is write containment, not isolation.** Reads, the network and process starts other than the three denied programs are open. A launchd job, a Keychain entry and a remote effect are `unknown`.
- **`sandbox-exec` is deprecated by Apple.** It works on the macOS this was built on (Darwin 27.2.0). There is no fallback without it.
- **Writes outside HOME are denied** except in the temporary directories and `/dev`. A script that must write elsewhere fails inside the script.
- **The window is shallow.** Top-level listings show a new or removed child, not a change inside it.
- **The script downloads more.** What it fetches after it starts is not pinned (`script-downloads`).
- **Fixed timeout.** One hour, with no flag.
- **No cleanup yet.** The window lists what changed. `purge` (DFM-82) and `adopt` (DFM-83) do not read it yet.

## Updates

A trial is updated with the other packages as if it were already adopted (the owner's decision of 2026-10-31). Nothing holds an update for a trial. A trial is not saved in the dotfiles configuration: it is a package that dfm manages from its own state. Updating a trial needs no entry for its tool in the dotfiles config. The `brew` updater, when the profile declares it, upgrades a Homebrew trial with the other outdated packages, and `packages --update` reads the trial record to roll a mise trial. No update writes the trial's tool into a dotfiles file (a mise configuration, a Brewfile, a manifest or a profile). Only `adopt` writes it into a module.

- **Homebrew.** `sync`'s `brew` updater upgrades a trial's formula or cask with every other outdated package (docs/upgrade.md, "Trials").
- **mise.** `dfm packages --update` rolls the tool of each active `mise/v1` trial as a declared rolling tool (docs/toolchains.md, "Trials"). The trial rolls run after every declared operation, so a failed trial roll cannot stop a declared update. `sync` runs `packages` without `--update`, so it rolls no mise tool, declared or trial.

The record is not changed by an update. It keeps what the trial saw at its start.

## Decisions

| Question | Choice |
| --- | --- |
| Command name | `trial`, the noun the state store uses for the record. DESIGN.md section 7 proposes `try`. |
| Ambiguous choices without a prompt | No command has a prompt. An ambiguous provider is `E_APPROVAL_REQUIRED` with `--provider` as the fix. A path choice is never ambiguous: the candidates are explicit, or one default. |
| Decline | The absence of `--yes-plan`: exit 3 `E_APPROVAL_REQUIRED`, as for `apply`. Exit 4 (`declined`) is reserved for an interactive prompt, which does not exist. |
| Existing configuration | The trial changes no configuration. A pre-existing candidate is recorded with its hash (`existed: true`), and the cleanup scope line says it is never removed. The tool's own first run may change it. The baseline hash attributes that change. |
| Configuration discovery | The providers do not know configuration paths. Without `--config`, the one candidate is `~/.config/<tool>` (a `shortcut:` in `candidates()`). |
| Native isolated configuration | Not used. No adapter declares one, and the trial does not start the tool. |
| Dependencies | Recorded in `package.install.dependencies` from the provider's own listing (brew only; mise lists none). A dependency may be shared, so purge must decide per dependency. |
| Pre-existing tool | Still recorded (baseline, owners, candidates) with a `current` receipt, so `adopt` can later promote its configuration. |
| Second trial of a tool | Refused (`E_CONFLICT`) while the record exists, also when its install was interrupted. `--id` names a second trial. |
| Receipt for a crash | None. A crash after the record or the intent leaves the record, the journal and `state check`'s finding. Only a run that reaches its end writes a receipt. |
| Signal handlers | Armed once, before the first probe. The gate test in `crates/cli/tests/gate.rs` lists `trial_cmd.rs` beside `apply_cmd.rs` as the only callers. |

## Known limits

- **Not an OS sandbox.** Services, caches, logs and any path outside the baseline are declared `unknown`. A path the tool creates there is a candidate for review at purge time, never an owned cleanup scope, and never gets a retroactive baseline.
- **One candidate by default.** A tool that keeps its configuration elsewhere needs `--config`. The record cannot be extended after the fact without a new baseline, so pass every candidate on the first run.
- **The baseline is one observation.** A hard-linked file is hashed like any other. `purge` must check again before it deletes.
- **Two ends of a trial.** `adopt` ends a trial by promoting it ([docs/adopt.md](adopt.md)); `purge` ends it by removing what it added ([docs/purge.md](purge.md)).
- **Rerun cost.** Two probe rounds (before and under the lock), plus the postcondition probe after an install.
- **Evidence scope.** Tests use the `test-isolation` build and the `brew` and `mise` look-alikes. No test runs a real install. Whether `sync` keeps active trials intact is not tested.

## What adopt and purge inherit

- **Attribution by one id.** `package.install.op_id` names the journal `intent` and `done` (or `failed`) entries and the receipt `<start>-<op_id>.json`. A trial without `package.install` installed nothing. An `op_id` with no `intent` entry means the run ended between the record and the intent: nothing was started, and purge treats the package as not trial-installed until the provider's listing says otherwise.
- **Baseline for restore.** `baseline.paths` with `existed` and `sha256` as observed before the install. `existed: false` is not delete authority: nothing observes the path after the install, so a file created there by anything else is indistinguishable from the tool's. Purge treats such a path as a candidate that needs its own approval (its preview shows the live hash and the approved digest covers it). It restores nothing for a pre-existing path whose hash no longer matches, and never touches a path with `existed: true` and no hash (a directory).
- **The installed version.** Purge removes the trial's package by name (`package.name`), every installed version, because updates may change it after the trial starts (docs/upgrade.md, "Trials"). A mise roll installs the new version beside the old ones and removes none. The record keeps what it saw at trial start as history and is not updated.
- **Owners.** `baseline.owners` says who had the tool. A pre-existing tool is never uninstalled. A dependency in `package.install.dependencies` is uninstalled only when no other active trial names it and Homebrew finds no installed formula that needs it. Purge reads no module (docs/purge.md, "Known limits").
- **Unknown effects.** Purge reports the `unknown` entries as retained or unattributed, never as cleaned.
- **Candidates for adopt.** `baseline.paths` are the files adopt reviews and promotes into a module. `package.adapter` and `package.name` are the install declaration it generates.
