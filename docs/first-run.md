# First live run (owner runbook)

This is the shortest safe path from the current integration branch to a first `apply` on a real machine. It covers links and the generated files that the release binary has adapters for. Nothing in this document runs by itself. Every step is a command the owner runs and can stop before.

Board ticket: DFM-46. The agent runs nothing live without written approval that names the plan digest. The rule is step 3 of section 4 below.

## 1. Build and install the manager

1. Place the pinned toolchain. `~/.local/share/dotfiles-manager` must be a real directory that you own and that group and others cannot write. Then run:

   ```sh
   (
     set -e
     mkdir -p ~/.local/share/dotfiles-manager/toolchains
     if [ -e ~/.local/share/dotfiles-manager/toolchains/1.98.1 ]; then
       echo 'stopped: remove toolchains/1.98.1 first, because cp would nest the copy' >&2
       exit 1
     fi
     cp -Rp ~/.rustup/toolchains/1.98.1-aarch64-apple-darwin ~/.local/share/dotfiles-manager/toolchains/1.98.1
     sh scripts/bootstrap.sh toolchain-digest --dir ~/.local/share/dotfiles-manager/toolchains/1.98.1
   )
   ```

   The block runs in a subshell. If it prints `stopped` or any error, or the status is not 0, do not go on to step 3.

   The copy must be a real directory, not a symlink, made as your own user and not with `sudo`: the script refuses a toolchain entry that another user owns (`E_TOOLCHAIN_MISMATCH`). The printed digest must equal `toolchain_tree_sha256` in `dotfiles-manager.lock` before `install`. The digest is device-specific. The M3 may need its own lock later.
2. Choose the commit to install. The integration branch is `going-to-sleep`. Use the full 40-character object id.
3. Run the bootstrap script (not with `env -i`: it clears `HOME` and the script refuses with `E_HOME_MISMATCH`; the script scrubs its own environment):

   ```sh
   /bin/sh scripts/bootstrap.sh install --source "$PWD" --commit <full sha>
   ```

   It checks Command Line Tools, the pinned toolchain, the staged tree and the build controls before any `cargo` process starts. It runs `cargo fetch --locked`, which needs network access, then builds offline, runs `self-check`, then installs `~/.local/bin/dfm` through a `current` symlink. A failed build leaves no installed binary.
4. Check: `dfm --version` prints the semver and the commit. The output must not contain `test-isolation`; the release binary ignores every isolation override.

## 2. Author a bundle in the configuration repository

The bundle lives in the dotfiles repository, not in this manager repository. Minimum layout:

```
<checkout>/
  base/bundle.toml
  base/modules/core/module.toml
  base/modules/core/files/...
  base/modules/m5/module.toml
  base/modules/m5/files/...
  profiles/m5-01.toml
```

`bundle.toml`:

```toml
schema = 1
id = "<name>"
core = "modules/core/module.toml"

[modules]
m5 = "modules/m5/module.toml"
```

A module with one link resource:

```toml
schema = 1
id = "m5"
platforms = ["macos"]
requires = []

[[resources]]
id = "zsh"
kind = "link"
adapter = "fs-link/v1"
target = ".zshrc"
[resources.value]
source = "files/zshrc"
```

Profile `profiles/m5-01.toml`:

```toml
schema = 1
id = "m5-01"
bundle = "../base/bundle.toml"
platform = "macos"
architecture = "arm64"
modules = ["m5"]
overrides = []
```

Rules that matter on the first run:

- `target` is relative to HOME and must be ASCII. The source file must exist inside the checkout.
- Know every file class the first apply writes. `link` resources (`fs-link/v1`) create symlinks. `render` resources (`git-config/v1`) write a whole file. `assemble` resources (`text-fragments/v1`) write a file built from fragments. A `merge` resource (`claude-settings/v1`, `.claude/settings.json` only) writes into a file that the application also owns: it sets the declared values and keeps foreign keys, and its first write into an existing file needs `--allow-adopt` because hand edits of managed fields are overwritten (docs/adapters.md). Apply also creates missing parent directories (mode 0700). A resource whose adapter is not registered in the release binary makes `apply` refuse the whole plan with `E_GENERATOR_UNAVAILABLE` (exit 3, nothing written).
- Do not list files under `.ssh` other than `config`, or any file that holds a secret. List `.claude/settings.json` only as a `claude-settings/v1` merge resource, never as a link, a render or an assemble resource; the planner refuses a sensitive link or removal.
- A checkout that holds ignored or runtime content (plugin checkouts, `.venv`, `node_modules`, caches) needs `[sources]` in the bundle. See the next section.
- A `link` resource may point at a file or a directory. A directory source needs `directory = true` in the resource value (`[resources.value]`); without it the resolver refuses with `E_MANIFEST_INVALID`. A directory link is one operation; the manager never writes through it and never deletes it recursively.

### Declare the source roots

Without `[sources]` the loader reads every file under the root. A live checkout often holds content the loader refuses: files over 1 MiB, a nested `.git`, more than 20,000 entries. Declare in the bundle which parts are sources. Example for a checkout with `home/` and `dfm/` at the top and the bundle in `dfm/`:

```toml
[sources]
roots = ["home", "dfm"]
exclude = [
  "home/.config/tmux/plugins",
  "home/.pi/agent/extensions/node_modules",
  "**/__pycache__",
  "**/.venv",
  "**/node_modules",
]
```

- Roots and patterns are relative to the root (`--root`, or the root the binding records). A pattern is anchored there, so `**/.venv` is the way to match a `.venv` at any depth.
- The profile, the bundle and every module must lie inside a root, or be a file or link directly beside the bundle file. A subdirectory beside the bundle is read only when a root holds it. Otherwise `plan` fails with `E_SOURCE_ESCAPE` and names the file.
- A resource source inside an excluded path fails with `E_SOURCE_ESCAPE` and names the pattern.
- A change to `[sources]` changes the source digest, so a saved plan goes stale.
- A link to a directory that holds excluded entries (for example a plugin directory with `.venv`) is published whole on apply. The plan shows `note: contains N excluded entries, not hashed` under that operation. Read it before you approve.
- A root may not be excluded, may not lie below an excluded path and may not name `.git`.
- If `plan` reports `E_LIMIT_EXCEEDED` for the entry count, the message names the three largest directories. Exclude them or narrow the roots.
- `plan --against` for a bundle without `[sources]` behaves as before.

## 3. Bind, then look without writing

```sh
dfm bind --profile <checkout>/profiles/m5-01.toml --machine-id <id> [--root <checkout>]
dfm plan
dfm doctor
dfm apply --preview
```

`bind` records the checkout root: `--root`, else the nearest directory above the profile that holds `.git`. Every later command uses that root when `--root` is not given; an explicit `--root` wins. A binding written before the root was recorded (schema 1) still works with `--root` on each command. To record the root, run the same `bind` again (same `--profile` and `--machine-id`): it rewrites the binding in place and journals the change.

`bind` writes only the binding file, the empty state store under `~/.local/state/dotfiles-manager` and the lock file `~/.local/state/dotfiles-manager.lock`. `plan`, `doctor` and `--preview` write nothing.

Read the plan. Each line is one operation:

| Line | Meaning |
| --- | --- |
| `planned mkdir@...` | a parent directory will be created (mode 0700) |
| `planned <resource>@<path> [module] link <path> -> <source>` | a new link will be created |
| `planned <resource>@<path> [module] write <path> with <adapter>` or `update <path> with <adapter>` | a rendered or assembled file will be created or replaced (`output: N bytes` and the setting names follow, never the content) |
| `planned <resource>@<path> [module] create <path> by merge ...` or `merge into <path> ...` | a merge into the application's file; `managed:` and `sets:` lines follow. `take over managed fields ...` needs `--allow-adopt '<id>'` |
| `planned <resource>@<path> [module] adopt <path>: ...` | an existing link already has the exact text; it will be recorded as owned, nothing written. Needs `--allow-adopt '<id>'` |
| `planned remove@...` | an owned output will be deleted. Needs `--allow-disruptive '<id>'` |
| `blocked ... E_CONFLICT` | a file exists at the target and the manager does not own it. Move it aside by hand, or change the profile |
| `current ...` | already correct |

`--preview` prints every flag the real run needs, ready to paste. A plan with any `blocked` line cannot be applied.

## 4. Back up, then approve

1. Make the list of targets from the plan:

   ```sh
   dfm plan --json > /tmp/plan.json
   python3 -c 'import json;[print(o["path"]) for o in json.load(open("/tmp/plan.json"))["plan"]["operations"] if o.get("path")]' > /tmp/targets.txt
   ```

2. Back up those paths and the manager state. `tar` skips paths that do not exist yet and says so:

   ```sh
   tar -czf ~/dfm-first-run-backup.tgz -C "$HOME" $(cat /tmp/targets.txt) .local/state/dotfiles-manager .config/dotfiles-manager
   ```

3. Approve in writing. The approval must name the plan digest printed by `plan`. Example: "Approved for apply on the M5: plan digest `<64 hex>`."

## 5. Apply

```sh
dfm apply --yes-plan <digest> [--allow-adopt '<id>']... [--allow-disruptive '<id>']...
```

- `-y <digest>` is the short form of `--yes-plan <digest>` on every command that takes it.
- A digest that no longer matches exits 3 with `E_PLAN_STALE` and writes nothing. Run `plan` again and approve the new digest.
- The last line is `result: <name>`. `ok` means every operation ended `updated` or `current`. Anything else: read the per-operation lines; the first failure stops further changes, and the receipt records what happened.
- A second run of the same plan changes nothing and exits 0.

Check afterwards:

```sh
dfm state check
dfm doctor
exec zsh
```

## 6. Undo

Deselect the module in the profile (`modules = []`), run `plan`, then `apply --preview`. It lists one `--allow-disruptive` flag per owned output. Apply with those flags removes the links the manager created or adopted and the rendered or assembled files it wrote that still match the ledger; the source files are untouched. A `claude-settings/v1` merge is retired without a flag: it removes only the hook entries the manager added, and the managed values stay in `.claude/settings.json`, which is never deleted. Files that existed before and were not adopted are never deleted. The backup from step 4 restores anything else.

## Known limits on the first run

- Links, rendered and assembled files, and the one `claude-settings/v1` merge. No secrets, no packages, no reloads of running programs.
- `Ctrl-C` during `apply` stops cleanly. The operation in flight finishes, the receipt has outcome `interrupted`, and the exit code is 130. SIGTERM gives `terminated` and 143. After SIGKILL, a signal with no handler (SIGHUP, SIGQUIT, ...) or a crash, the next run settles the interrupted operation from the journal before it does anything else. A merge that was interrupted after its rename cannot be settled that way: its path stays blocked with `E_OP_INTERRUPTED` in every run until `dfm state resolve <journal-id> --as failed` (docs/apply.md, "Known limits").
- A removal or link replacement checks the leaf and then acts in a second step. A concurrent same-user process could change the leaf in between.
- Config edits in the checkout never require rebuilding the manager. Only a new manager commit does, through `bootstrap.sh install`.
