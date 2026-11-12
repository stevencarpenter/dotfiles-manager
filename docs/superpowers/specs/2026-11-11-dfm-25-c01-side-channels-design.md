# DFM-25 C01: move the side-channel installers into dfm

Status: draft for owner review, 2026-11-11; owner answers of 2026-11-11 (fantasy-sports as an external bundle, mise gated) applied. Kaneo: DFM-25 (C01; the grant rule is C04). Inputs: the code at main 0e90709 and `~/.dotfiles/scripts/sync-side-channels.sh` at the dotfiles `main` of 2026-11-11.

## Intent

`dfm sync` runs the `side-channels` updater (PR #100). That updater runs the bound checkout's `just sync-side-channels` recipe, which runs `scripts/sync-side-channels.sh`. The script fetches code from about nine origins and none of them has a grant. A commit pulled into `~/.dotfiles` can therefore add an install from a new origin, and the next sync runs it. The DFM-25 trust design (`2026-11-02-dfm-25-extension-trust-design.md`, G2) forbids this. It deferred the side-channel installers to C01 and gave each of them a grant check "once they move". Codex raised the gap as a P1 on PR #100, and the owner accepted it until C01 lands.

The owner decided on 2026-11-11 that every step of the script is managed by dfm. This widens the C01 narrowing of 2026-11-03 ("2A"), which had put tpm out of scope. When this design is delivered, `scripts/sync-side-channels.sh`, its `sync-side-channels` recipe and the `side-channels` updater are deleted.

## Goals

- **G1.** Every step of `scripts/sync-side-channels.sh` is a declared resource in the dotfiles bundle or a built-in updater of dfm. The inventory below is the checklist.
- **G2.** Every external origin that a step fetches from needs a grant in `state/grants.json` before the first fetch, with the identities of docs/sync.md ("Source identities"). An ungranted source gives a `blocked` row with the `dfm sync --trust-origin` command, as the `skills` and `agents` updaters do now.
- **G3.** A pin (a tag, a commit, an installer version) is data in a bundle document, not a constant in Rust. Changing a pin never needs a rebuild of dfm (owner constraint).
- **G4.** A profile still cannot add a command. Resources and updaters come from the fixed set built into dfm. A document chooses only which ones run and with which data.
- **G5.** At the end, `scripts/sync-side-channels.sh`, the recipe, `Updater::SideChannels` and its tests and docs are removed, and the `side-channels` entry is removed from `personal-mac.toml`.

## Non-goals

- The other steps of the dotfiles `sync` recipe: `scripts/launch-agents.sh`, `scripts/login-shell.sh`, `scripts/tiling-reload.sh` and `scripts/mac-defaults.sh`. They fetch nothing.
- Pinning sources that are rolling now. The owner's model is rolling by default. A source that the script pins stays pinned, and a source that it rolls stays rolling.
- Moving the scripts that the steps call (`install-agent-registry.sh`, the Firstmate entry point) into Rust when they run code from the bound checkout only. The bound checkout comes from a granted origin. That reasoning holds for local code. It does not hold for a remote fetch, which this design gates.
- The yazi `package.toml`, the nvim `lazy-lock.json` and the Pi npm extensions. The script does not install them.

## Anti-goals

- A generic "run this command" resource or updater. It would let a document add a command, and that breaks G4.
- A grant prompt on every sync. A grant is written once per identity per machine, as now.
- A new trust store. Grants stay in `state/grants.json`.
- A cutover that loses a step. Until a step is delivered by dfm, it stays in the script. The script gets shorter with each slice and is deleted in the last one.

## Inventory and target

| # | Script step | Origin(s) | Target in dfm |
| --- | --- | --- | --- |
| 1 | `mise install` | mise's registries and the `github:`, `npm:` and `go:` backends that the global config names | The `mise` updater runs `mise install` before `mise upgrade`, and both are gated (see "mise sources"). |
| 2 | `mise upgrade --no-prune` | the same | The existing `mise` updater. Its prune behavior stays (see "Decided in this design"). |
| 3 | DuckDB `INSTALL spatial; INSTALL avro;` | DuckDB's core extension repository | New package adapter `duckdb-extension/v1`. |
| 4 | gh-axi skill: `npx --yes skills@1.7.0 add <github tree URL at a commit> --skill gh-axi --global --yes --agent ...` | `npm:skills`, `https://github.com/kunchenguid/gh-axi` | New package adapter `skill-source/v1`. |
| 5 | `firstmate --setup` | `https://github.com/kunchenguid/firstmate` | A `git-checkout/v1` resource for `~/.local/share/firstmate/repo`. `--setup` is no longer run (see "Firstmate"). |
| 6 | `op-render` with `op signin` on a TTY | 1Password | `~/.config/zsh/.personal.env` is already an `op-template/v1` secret (`dfm-module-personal.toml`). `~/.ssh/config.d/10-homelab.conf` is declared the same way. op-render wrote it, so the first render is a takeover that needs one `--allow-adopt`. No dfm code change. |
| 7 | tpm clone or `pull --ff-only` | `https://github.com/tmux-plugins/tpm` | New package adapter `git-checkout/v1` and a new `checkouts` updater. |
| 8 | Agent registry clone over SSH, then `install-agent-registry.sh` | `ssh://git@github.com/stevencarpenter/agents` | `git-checkout/v1` for the managed copy, and the existing `agents` updater declared in the profile. |
| 9 | `install-fantasy-sports.sh`: clone over SSH, copy one agent file to three agent directories and one skill tree to Cursor | `ssh://git@github.com/stevencarpenter/fantasy-sports` | The fantasy-sports repository becomes a dfm external bundle, fetched by sync under an origin grant (see "Fantasy-sports delivery"). |
| 10 | `lefthook install` in the bound checkout | none | New `lefthook` updater: the fixed command `lefthook install` in the bound checkout. |
| 11 | token-auditor: `uv tool install --force git+https://github.com/stevencarpenter/token-auditor@<tag>`, with the tag from `versions/token-auditor` | `https://github.com/stevencarpenter/token-auditor` | `uv-tool/v1` accepts a git source with an immutable ref. |
| 12 | agent-reap: `uv tool install --force --reinstall <checkout>/agent_reap` | none (local) | `uv-tool/v1` accepts a path source inside the bound checkout. |
| 13 | Codex ponytail: `codex plugin marketplace add DietrichGebert/ponytail`, `codex plugin add ponytail@ponytail` | `https://github.com/DietrichGebert/ponytail` | New package adapter `codex-plugin/v1`. |
| 14 | `firstmate --request-update` | Firstmate's own origin, fetched by Firstmate | New `firstmate` updater: the fixed command `~/.local/bin/firstmate --request-update`. |

## The parts

### Package adapters

A new package adapter follows docs/packages.md: a read-only probe before the plan, an install operation in the packages scope, a re-probe after the install, and an `E_MANIFEST_INVALID` refusal of bad fields at resolve time. Each adapter that fetches from an origin reports its identities, and the packages scope blocks the operation without a grant, as `script/v1` does with `script:<url>` (docs/sync.md, "Source identities").

- **`git-checkout/v1`.** Fields: `url` (a git URL that maps to a canonical identity), `path` (HOME-relative), `branch` (optional; the clone uses `--branch <branch>`). The probe passes when `<path>/.git` exists and `remote.origin.url` maps to the same identity. A different identity is `E_CONFLICT` (another owner). A missing path is installed with `git clone <url> <path>`. The packages scope never pulls. The identity is the origin of `url`.
- **`skill-source/v1`.** Fields: `source` (the GitHub tree URL at a commit, as the script writes it), `skill`, `agents` (a list), `installer` (an `npm:skills@<version>` value). The probe reads the `skills` CLI's source record, as `skills_sources` does now, and passes when an entry has that `source` and `skill`. The install runs `mise exec -- npx --yes skills@<version> add <source> --skill <skill> --global --yes --agent <agents...>` with the default npm registry forced, as the `skills` updater does. Identities: `npm:skills` and the repository origin of `source`.
- **`codex-plugin/v1`.** Fields: `marketplace` (a GitHub shorthand or URL), `plugin` (`<name>@<marketplace>`). The probe reads `codex plugin marketplace list` and `codex plugin list`. The install runs `codex plugin marketplace add` when the marketplace is missing, then `codex plugin add`. The identity is the marketplace origin. A marketplace grant covers its plugins, as for Claude (trust design, "Grants").
- **`duckdb-extension/v1`.** Field: `name`. The probe runs `duckdb -init /dev/null -c` with a query on `duckdb_extensions()` and passes when the extension is installed. The install runs `INSTALL <name>`. DuckDB keeps extensions per version, so a DuckDB upgrade makes the probe fail and the next sync installs the extension again. DuckDB's core repository is the tool's own owner, so it is not gated, as for `brew` and `mise`. `DUCKDB_AGENT_MODE=0` is set, as in the script.
- **`uv-tool/v1` sources.** Add an optional `source` field. `git+https://<repo>@<ref>` needs a grant for the repository origin, and the ref must be a tag or a full commit, never a branch. The install runs `uv tool install --force <source>`. A `path:<bundle-relative dir>` source must resolve inside the bound checkout and needs no grant. The probe compares the source that uv records for the tool with the declared source; a different source is stale and is installed again. A name-only value keeps its current behavior.

### Updaters

- **`checkouts`.** For each declared `git-checkout/v1` resource except the agent registry's managed copy and Firstmate's checkout, it runs the fast-forward flow of the `agents` updater: blocked when the copy is dirty, has no upstream, has an ungranted upstream remote or has diverged. It never merges, resets or clones. Statuses follow `agents`: `updated` on a changed revision, `current` otherwise. One row per checkout, or one row with one note per checkout (left to the builder).
- **`agents`.** Unchanged, but the profile now declares it. The managed copy comes from a `git-checkout/v1` resource, so the updater still never clones. The `agents` updater keeps the pull of its copy, and `checkouts` skips that copy, so the install never runs from a revision that another updater has not pulled yet. The working-copy rule stays.
- **`mise`.** Runs `mise install` before the existing flow.
- **`lefthook`.** Runs `lefthook install` in the bound checkout. A missing `lefthook` is `blocked`. A failure is `failed` and stops nothing, as in the script.
- **`firstmate`.** Runs `~/.local/bin/firstmate --request-update` and reports `pending` (an asynchronous request is not completion). It is declared last and runs only after the other updaters. A missing entry point is `blocked`.
- **`side-channels`.** Deleted in the last slice.

### Fantasy-sports delivery

Owner decision, 2026-11-11: the fantasy-sports repository (`ssh://git@github.com/stevencarpenter/fantasy-sports`) becomes a dfm external bundle, as the work overlay is (`examples/external/bundle.toml`). The repository gets a bundle document and a module that declares the four destinations as `fs-link/v1` resources: `agents/fantasy-football-manager/agent.md` to `~/.claude/agents/`, `~/.cursor/agents/` and `~/.config/opencode/agents/`, and `skills/fantasy-football-guidelines/` to `~/.cursor/skills/`. The dotfiles lock names it as a nested source, so sync checks its origin grant and fetches it before the config scope. The personal profile lists its module, and the work profile does not.

- The bundle is pinned in the lock. New fantasy-sports content reaches a machine through a lock bump, not a roll. The owner accepted this.
- The four destinations hold copies that the script wrote. A regular file or a directory at a link target is `E_CONFLICT` (docs/planner.md), so the owner moves the four copies aside once, by hand, in the slice that adds the bundle. The slice's dotfiles PR description gives the four commands.
- The working copy (`~/projects/fantasy-sports`), the mcp-sync cache and the managed copy (`~/.local/share/fantasy-sports`) are no longer used. The managed copy can be deleted after the cutover.
- Whether Cursor follows a link for an agent file or a skill directory is unchecked. The slice checks it on the M5 before the dotfiles PR merges.

### mise sources

Owner decision, 2026-11-11: `mise install` and `mise upgrade` are gated. Before any mise command, the `mise` updater finds the mise files without running mise: in the runner directory under HOME and each parent up to `/`, `.tool-versions`, `mise*.toml` and `.mise*.toml` (`mise.toml`, `mise.local.toml`, `mise.<env>.toml`), `.config/mise*.toml`, and `config*.toml` and `mise*.toml` in `.config/mise/`, `.mise/` and `mise/` with every `conf.d/*.toml` there; then the same names in `~/.config/mise/` and `/etc/mise/`. mise also reads files below a `conf.d` (such as `conf.d/<dir>/mise.toml`), which the gate does not enter: any `conf.d` entry that is not a `*.toml` file (a directory, a dangling link, a special file or another name) blocks the row with a note that names it. A link to a `*.toml` file is read as the file it names, because the dotfiles link these files from the checkout. It reads each one (a `.tool-versions` file as `<tool> <version>` lines) and maps each tool of a non-registry backend to an identity:

- `github:o/r` and `ubi:o/r` map to `https://github.com/o/r`.
- `npm:<name>` maps to `npm:<name>`.
- `go:<module path>` maps to its own identity `go:<module path>`, stored in `packages` beside `npm:`. A module path is a lowercase host with a dot and a path, with no version.
- A tool of another backend (`vfox:`, `cargo:`, `pipx:`, `aqua:`, `asdf:` and the others) is handled as the builder finds the registry maps it: a backend with no mapping is `E_SOURCE_UNTRUSTED`. A short name of mise's registry (`go`, `java`, `rust-analyzer`) is not gated, because mise's registry is its owner, as Homebrew's official formulae are.

Each of these also blocks the row, with one note that names the file and the entry, because each one can move a fetch away from the checked identity:

- A file that cannot be read or parsed.
- A template (`{{`, `{%` or `{#`): mise renders it to read the file, and a template can run a command.
- A non-empty `[hooks]` table: `mise install` runs its commands.
- A `conf.d` entry that is not a `*.toml` file, because mise reads files below it.
- A non-empty `[plugins]`, `[tool_alias]` or `[alias]` table.
- A tool option other than `version` or `minimum_release_age` in a value table, or other than `exe`, `bin` or `matching` in a `[...]` key suffix.
- Any setting under `[settings]`, at any depth, other than `trusted_config_paths`.
- A non-empty `[env]` table: mise passes it to the commands it runs, so `npm_config_registry` or `GOPROXY` there would move a granted fetch.
- A lockfile beside a gated file (`mise.lock`, `mise.local.lock`, `mise.<env>.lock`, `.mise*.lock` or `<stem>.lock`, and for a `conf.d` file the directory above), because it can record a download url.

Only when every file passes and every identity has a grant does `mise config ls --json` run, with the runner, directory and environment of `mise install`. A listing that fails, or a path it lists that the gate did not read, blocks the row before the install.

Every mise command (`config ls`, `ls`, `install`, `outdated`, `upgrade`) runs with the default npm registry, also for each `npm:@scope` the files name, so an `npm:` grant covers that registry only. It also runs with Go at its defaults: `GOENV=off` (so Go's own env file from `go env -w` is not read), `GOPROXY=https://proxy.golang.org,direct`, `GOSUMDB=sum.golang.org`, `GOTOOLCHAIN=local`, and `GOFLAGS`, `GONOSUMDB`, `GONOSUMCHECK`, `GOPRIVATE`, `GONOPROXY` and `GOINSECURE` empty. A `go:` grant names a module, and the public Go proxy serves it.

`mise ls --installed --json` runs before and after `mise install`. Each tool version the install adds is a note and makes the row `updated`.

The `skills` updater runs `mise exec`, which renders the same files. Before it, the same file checks run (the files, the `conf.d` rule, templates, `[hooks]`, `[env]`, whose directives such as `_.source` run a script, read and parse errors), without the backend-tool grants. `mise exec` gets the same environment, and `MISE_EXEC_AUTO_INSTALL=false` and `MISE_NOT_FOUND_AUTO_INSTALL=false`, so it installs no configured tool.

One ungranted identity blocks the whole row before any mise command that fetches, with one note per identity, as for `skills` (trust design, anti-goal "a partial updater run"). This changes DFM-85's rule that mise is not gated. docs/upgrade.md ("Source trust") and docs/toolchains.md change with it. The current live tools need grants once, on the first sync after the slice; the slice's PR description lists the `--trust-origin` commands for the M5's current config.

### Firstmate

dfm clones Firstmate itself with a `git-checkout/v1` resource (`url = "https://github.com/kunchenguid/firstmate"`, `branch = "main"`), so the grant covers the URL that is fetched. `firstmate --setup` is not run: its clone step is skipped when the checkout exists, and its other step migrates a legacy detached install that no machine has after this change. The wrapper's own origin check (`~/.local/bin/firstmate`) refuses to launch a checkout with another origin, so a wrapper and a declaration that disagree fail closed. Firstmate pulls its own checkout (`--request-update`, step 14), so `checkouts` skips it. `firstmate-native/v1` stays a catalog entry with no adapter.

### Order inside one sync

No ordering between secrets and the SSH clones is needed. `10-homelab.conf` defines only `Host i9`, and GitHub SSH uses the `Host *` `IdentityAgent` block of `~/.ssh/config`. The script's comment about rendering before the clone is stale.

The updaters run after the four scopes. The tools that the new package adapters call (`duckdb` and `npx` from mise, `codex` from its native install) can therefore be missing on a fresh machine's first sync. That adapter's row is then `blocked` with the missing tool named, and the second sync installs it. This is accepted.

## Decided in this design (my calls, open to the owner)

- The `mise` updater keeps mise's prune grace period (`upgrade.prune_after`, 24 h) instead of `--no-prune`. The script's reason is to keep old installs for sessions that are already running, and the grace period covers that.
- The `agents` updater's working-copy rule is kept because it exists in dfm now.
- `versions/token-auditor` is removed. The tag moves into the `uv-tool/v1` value.
- The `skills@1.7.0` installer pin moves into the `skill-source/v1` value.
- agent-reap is installed by the packages scope when its declared source differs from uv's record. The `uv` updater runs `uv tool upgrade agent-reap` on every sync, as for every tool. Whether that rebuilds a path tool whose code changed without a version bump depends on uv's cache and is unchecked. If it does not, the builder adds a content digest of the directory to the probe.
- Known gap: a path source's own dependencies can name `git+` direct references, and the install fetches them without a grant.
- Capability gates of the script (`host-capability.sh skills`, `mcp`, `agents`, and the `personal` identity) become module membership. A resource is in a module that the work profile does not list.
- The `skill-source/v1` probe does not use the `skills` CLI's source record: that record has no gh-axi entry today, and an entry there would let the `skills` updater roll the skill past its pin. The probe passes when the installed skill directory exists and dfm's own record of the last install (state, keyed by resource) names the declared `source` and installer version.
- The other callers of the script are rewired in the last dotfiles PR: the `sync-dfm-apply` recipe (`Justfile:92`), `scripts/update-inputs.sh:42`, `scripts/test-sync-capability-gating.sh` and the message at `bootstrap.sh:106`.
- Each dotfiles PR removes a step from the script in the same change that declares its resource, so no file has two writers after the merge (op-render and the secrets scope included).
- `op signin` is not run. Without a 1Password session the secrets scope is skipped with its warning, as now, and the owner runs `op signin` and syncs again.

## Left to the builder

- Field names, row layout and note text, within the docs' existing style.
- Whether `checkouts` reports one row per checkout or one row with notes.
- The test layout. Required cases: each new adapter's probe for installed, missing and other-owner states; each fetching adapter blocked without a grant and installed with one; `uv-tool/v1` refusing a branch ref and a path outside the bound checkout; `checkouts` blocked on a dirty, diverged and ungranted copy; a skill installed at another source than declared reinstalled; the `mise` updater blocked before any mise command by one ungranted backend tool, and run when every identity has a grant.
- The docs: `docs/packages.md`, `docs/upgrade.md`, `docs/sync.md` ("Source identities", the updater list), `docs/adapters.md` (the table), `README.md` and the `dfm sync` help text.

## Delivery

Each slice is one PR that passes the gate and gets Codex's thumbs-up. Each slice removes its steps from `scripts/sync-side-channels.sh` in a matching dotfiles PR, which merges only after the M5 runs a dfm that has the slice (`dfm self-update`).

1. `uv-tool/v1` sources (steps 11 and 12), and the gated `mise install` (step 1, "mise sources").
2. `git-checkout/v1` and the `checkouts` updater (step 7), then the agent registry (step 8), and the bundle's `op-template/v1` secret (step 6).
3. The fantasy-sports external bundle (step 9).
4. `skill-source/v1` (step 4) and `codex-plugin/v1` (step 13).
5. `duckdb-extension/v1` (step 3), `lefthook` (step 10), Firstmate (steps 5 and 14), then deletion of the script, the recipe and `Updater::SideChannels` (G5).
