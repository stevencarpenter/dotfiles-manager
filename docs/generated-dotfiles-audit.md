# Generated dotfiles audit

**Stow can replace the linking layer, but it cannot replace this repository's configuration generation and merge behavior by itself.** Most of that behavior already exists in standalone Python and shell tools. The Nix-specific user-configuration work is four activation-time emitters and one small capability-dependent file, not a large general-purpose template library.

Audit date: 2026-10-05. Current source revision: `1dbd12d3a563`.
Historical comparison: `16f2060a3a00`, the parent of the Nix cutover commit `ff786a0`.

Scope: the tracked public repository and the evaluated `personal-mac` configuration. External work flakes and external agent-generator implementations are not included. Their local integration points are included. No live secret contents were read, no vault operations ran, and no system switch, package update, or live configuration regeneration ran. The existing `.gitleaks.toml` change from the preceding secret audit was preserved.

## 1. Measured inventory

| Inventory | Verified count | Evidence |
| --- | ---: | --- |
| Home Manager path entries for `personal-mac` | 126 | Offline evaluation of `home.file` |
| Source-based entries, implemented as out-of-store links | 115 | Evaluated sources and the owning link declarations |
| Empty marker entries | 9 | Seven explicit repository seams, plus Home Manager cache/state markers |
| Nonempty `home.file.text` entries | 2 | SketchyBar `machine.env` and Home Manager's font-version marker |
| MCP output destinations | 11 | `sync_destinations`: nine wholesale JSON outputs and two patched outputs |
| Other activation-written user config/instruction outputs | 4 | Claude settings, Codex hooks, Codex instructions, Pi instructions |
| 1Password render targets | 2 | `home/.config/op/render-manifest` |
| Current Python configuration base templates | 2 | `codex.base.toml` and `opencode.base.json` |
| Current `.tpl` files | 2 | Personal environment and homelab SSH fragment |
| Historical `.tmpl` files | 25 | `git ls-tree` of `16f2060a3a00` |

These counts have different scopes and must not be added together. A Home Manager entry may link a directory containing many files. Activation and Python outputs generally do not appear in `home.file`. The historical `.chezmoiignore` also contains template directives despite having no `.tmpl` suffix.

The fixed user-facing render/merge/assembly inventory below has 18 destination paths: 11 MCP destinations, four other activation-written outputs, two secret targets, and one SketchyBar flag file. This excludes framework markers, system provisioning, copied skills/agents, and runtime caches. A registered destination is not proof that a fresh machine has already provisioned it; the Claude MCP patch skips a missing `~/.claude.json`.

Evidence: `modules/home/dotfiles.nix`, `modules/home/dev-tools.nix`, `modules/home/tiling.nix:30`, `hosts/personal-mac.nix`, `mcp_sync/src/mcp_sync/sync.py:1050`. Evaluated metadata is saved under `.lavish/generation-audit/`.

## 2. Configuration content that actually needs generation

### MCP configurations: 11 destinations

Writer: `mcp_sync`, a standalone Python project. Nix invokes it; Nix does not implement its transformations.

Inputs and behavior:

1. Load `~/.config/mcp/mcp-master.json` and merge the selected machine overlay.
2. Apply each client's server overrides and convert the common server schema to that client's schema.
3. For wholesale outputs, merge the client's base template, generated configuration, and non-server overrides. Expand `HOME` and XDG variables in base templates.
4. Filter disabled/retired servers. Optionally discover local oMLX models for OpenCode, retaining template fallback models.
5. Publish complete files atomically. Patch application-owned files rather than replacing their unrelated settings.

Nested objects merge recursively. Normal arrays replace. A key with a `+` suffix appends unique list items. These semantics are not supplied by Stow.

#### Wholesale outputs

| Consumer | Destination |
| --- | --- |
| Copilot CLI | `~/.copilot/mcp-config.json` |
| Copilot IntelliJ | `~/.config/github-copilot/intellij/mcp.json` |
| Copilot shared config | `~/.config/github-copilot/mcp.json` |
| OpenCode | `~/.config/opencode/opencode.json` |
| Cursor | `~/.cursor/mcp.json` |
| VS Code | `~/Library/Application Support/Code/User/mcp.json` |
| Junie | `~/.junie/mcp/mcp.json` |
| LM Studio | `~/.lmstudio/mcp.json` |
| Shared MCP config used by Pi | `~/.config/mcp/mcp.json` |

Evidence: `mcp_sync/src/mcp_sync/sync.py:968`.

#### Co-owned outputs

| Consumer | Destination | Ownership behavior |
| --- | --- | --- |
| Codex | `~/.codex/config.toml` | Seed from the base template on a fresh machine. Replace managed MCP tables and enforce template `[tui]` keys. Preserve unrelated live configuration and unmanaged MCP tables. |
| Claude | `~/.claude.json` | Patch managed `mcpServers` entries while preserving unmanaged servers and application keys. Explicit non-server overrides can also change configured keys. Skip a missing target. |

These files can contain application-owned state. Replacing them with static symlinks would not preserve the current behavior.

`--check` computes drift without writing destinations. `--capture` can persist supported live edits into local per-client overrides and verify that a fresh render reproduces them. Codex capture is explicitly unsupported. Live model discovery is excluded from drift/capture rendering.

Current triggers: the `mcpSync` Home Manager activation hook and `just mcp-sync`. Master and overlay edits change the linked inputs immediately, but destination files still require a sync.

Evidence: `sync.py:38`, `:122`, `:189`, `:339`, `:385`, `:645`, `:745`, `:1050`, `:1075`; `omlx.py:86`; `drift.py:120`; `capture.py:191`; `modules/home/sync-hooks.nix:39`; `Justfile` recipe `mcp-sync`.

### Claude settings: structured merge, not file templating

Output: `~/.claude/settings.json`.
Writer: `claudeSettingsMerge` in `modules/home/ai-stack.nix:64`.

Inputs:

- `home/.claude/settings-base.json`.
- Nix-generated capability/identity variants: plugin enablement and personal/routing SessionStart hooks.
- `~/.claude/settings.d/*.json`, merged in lexical order with later fragments winning.
- Existing live settings, including in-tool preferences and tool-installed hooks.

The managed settings override owned live keys. Unknown live keys survive. Hook handling is more specific than recursive JSON merging: managed handlers are enforced, retired or disabled owned handlers are removed, and foreign command/non-command handlers and group metadata are preserved. Model and effort defaults are seeded only when absent. Required sandbox write paths are appended without deleting user additions.

Current trigger: Home Manager activation after `linkGeneration`. Editing the linked base or a fragment does not itself rewrite the live settings file.

Stow implication: this needs an executable merge operation with the existing ownership rules. A generic text template would not provide equivalent preservation behavior.

Evidence: `modules/home/ai-stack.nix:42`, `:76`, `:83`, `:101`, `:152`, `:159`, `:168`; `scripts/test-claude-settings-order.sh`; `scripts/test-claude-hooks-preservation.sh`.

### Codex and Pi instructions: two concatenated outputs

| Output | Inputs |
| --- | --- |
| `~/.codex/AGENTS.md` | `home/.codex/AGENTS.md` plus `~/.codex/AGENTS.d/*.md`, appended in lexical order |
| `~/.pi/agent/AGENTS.md` | The same Codex body plus `~/.pi/agent/AGENTS.d/*.md`, including Pi runtime guidance and the shared output-policy fragment |

Writers: `codexAgentsAssemble` and `piAgentsAssemble`. Both write temporary files and rename them into place. These destinations are derived files, not live source links. Direct edits to the destinations are replaced on the next assembly.

The shared `home/.config/agents/output-policy.md` is itself plain source. Nix links it into both fragment directories. Claude and OpenCode consume the policy through their own instruction mechanisms rather than these two assemblers.

Current trigger: activation after `linkGeneration`. A shared body or policy edit can therefore be live for one consumer but stale for the two assembled consumers until activation runs.

Stow implication: linking the source fragments is supported; concatenating them is a separate operation. Neither assembler intrinsically needs Nix evaluation.

Evidence: `modules/home/dotfiles.nix:293`, `:297`, `:305`, `:333`.

### Codex hooks: preserve a writable, co-owned document

Output: `~/.codex/hooks.json`.
Writer: personal-only `codexHooksMerge`.

The hook preserves the existing document, converts a symlink to a writable regular file when necessary, and inserts the personal Hippo SessionStart command only if that exact command is absent. The codebase-memory installer also writes this target.

Current trigger: activation after `linkGeneration`.

Stow implication: this is another merge operation, not a file that should be replaced wholesale by a repository link.

Evidence: `modules/home/dotfiles.nix:362`.

### SketchyBar: one generated capability flag

Output: `~/.config/sketchybar/machine.env`.
Input: `caps.sketchybar_workspace_badges` in `lib/machines.nix`.
Current value: `SKETCHYBAR_WORKSPACE_BADGES=0` for `personal-mac`.

Writer: a small `home.file.text` declaration. This is the only repository-authored, nonempty `home.file.text` configuration in the evaluated host. The other nonempty text entry is Home Manager's font-version marker.

Current trigger: Nix generation and activation. The flag file must remain reachable from the existing SketchyBar launcher/plugin path arrangement.

Stow implication: a selected static file or a one-value emitter can represent this behavior. This does not justify a general template engine by itself.

Evidence: `modules/home/tiling.nix:30`, `lib/machines.nix`, evaluated `home.file` metadata.

### 1Password: two intentionally local plaintext outputs

| Source | Output | Generated content |
| --- | --- | --- |
| `home/.config/zsh/.personal.env.tpl` | `~/.config/zsh/.personal.env` | 15 exported variables injected from approved 1Password references |
| `home/.ssh/config.d/10-homelab.conf.tpl` | `~/.ssh/config.d/10-homelab.conf` | SSH fragment with an injected hostname |

Writer: `home/.local/bin/op-render`, using `op inject`.

Rendering is separate from Nix activation. It requires an authenticated 1Password context, writes through same-directory temporary files, applies mode `0600`, and preserves the prior target on failed or empty injection. The success sentinel records a complete manifest render. The static policy validates references, manifest entries, target ignore rules, and the Git boundary.

Current triggers: the personal side-channel sync or direct invocation of `op-render`. Activation performs only a stale-render warning. It does not contact 1Password.

Stow implication: retain this renderer. Neither template nor rendered secret should be blindly deployed as the application's configuration through a broad Stow package. Rendered targets must remain local regular files outside the repository. The templates are inputs to `op-render`, not substitute configuration files.

Evidence: `home/.config/op/render-manifest`, `home/.local/bin/op-render`, `home/.local/bin/op-adopt`, `modules/home/sync-hooks.nix:26`, `scripts/sync-side-channels.sh:54`. The preceding secret audit found no tracked ciphertext or rendered targets and verified mode `0600` for both live outputs. Historical ciphertext is still in Git history.

## 3. Distribution and runtime-derived content

These operations create files, but most do not render dotfile templates.

| Operation | Inputs and outputs | Current trigger and owner |
| --- | --- | --- |
| Skill fan-out | Master manifest plus personal overlay resolve to 13 skills: 12 local source links and one Git-sourced copy, deployed to Claude and Pi. Cache, ownership markers, and `skills-state.json` support refresh and cleanup. | `skillsSync` activation; standalone `sync-skills` CLI. Existing Python implementation. |
| Patched/vendored skill links | Selected repository skill trees are linked directly into multiple harnesses. They are distinct from manifest-managed skill paths. | Nix link declarations in `dotfiles.nix`. Linking, not content generation. |
| Personal agent registry | External CLI installs agent definitions and a manifest. The local wrapper additionally generates `~/.cache/agent-routing/context.md` atomically and validates installed tool allowlists. | Personal `agents` gate in side-channel sync; `scripts/install-agent-registry.sh`. External generator implementation. |
| Fantasy-sports distribution | Copy one agent Markdown file into Claude, Cursor, and OpenCode, plus a Cursor skill tree. Claude/Pi skill delivery belongs to `sync-skills`. | `fantasySportsSync` activation and side-channel sync both call the installer. Copies, not text rendering. |
| Shell initialization cache | Cache output of Homebrew, zoxide, Atuin, and Worktrunk shell-init commands under `~/.cache/zsh-eval-cache/`. Invalidate using binary/config metadata. | Shell startup through `zcached`. Independent of Nix/chezmoi apply. |

Evidence: `mcp_sync/src/mcp_sync/skills.py:264`, `:390`, `:534`, `:648`, `:665`; `home/.config/skills/{skills-master.json,machine/personal.json}`; `scripts/install-agent-registry.sh`; `scripts/install-fantasy-sports.sh`; `home/.config/zsh/lib/eval-cache.zsh`.

Other derived artifacts have separate lifecycles:

- The Pi oMLX extension discovers and registers provider models at startup/refresh. The source keeps a fallback catalog; Pi owns its runtime persistence. OpenCode's model discovery instead runs during MCP sync. Evidence: `home/.pi/agent/extensions/omlx-discovery.ts`, `mcp_sync/src/mcp_sync/omlx.py`.
- The SketchyBar menu-hover launcher compiles a Swift helper into `~/.cache/sketchybar/` when missing or older than the source. It also manages the helper's launch lifecycle. Evidence: `home/.config/sketchybar/plugins/menu_bar_hover.sh`.
- `just site` builds the repository atlas under `site/` from tracked source and static assets. It is documentation generation, not deployed home configuration. Evidence: `scripts/build-site.py:865`.
- LazyVim's `lazy-lock.json`, Pi package settings, npm/uv lockfiles, and the Powerlevel10k wizard output are tracked application/tool-maintained artifacts. They are not Nix/chezmoi render outputs that require a replacement template engine.
- Firstmate's checkout and native-update state remain application-owned. The repository links only its launcher and selected declarative config files. Evidence: `home/.local/bin/firstmate` and `docs/adopting-a-config.md`.

## 4. Configuration that looks generated but is already plain or natively layered

| Area | Current mechanism | What remains beyond linking |
| --- | --- | --- |
| mise | Raw `config.toml` plus capability-selected raw `conf.d/*.toml` | Tool-native TOML layering; installation/upgrades are separate |
| AeroSpace | Select `aerospace.personal.toml` or `aerospace.work.toml` for the standard target name | Identity-based source selection and application reload |
| Atuin | Select `config.sync.toml` or `config.local.toml` | Capability-based selection; keep history/state outside the checkout |
| Git, SSH, tmux, zsh | Native includes, fragment globs, or sourced `profile.d` files | Preserve include order and external fragment ownership |
| Claude global instructions and shared policy | Plain linked instruction files; instruction imports | No complete-file renderer for the Claude body |

The capability table is an installation/selection policy, not evidence that every selected file is templated. This repo declares one host. External wrappers can supply different identities, capabilities, and source roots through the exported flake API.

The locked v1.0 external-overlay contract owns extension namespaces, include order, and the caller-provided machine configuration. A personal-host linker change does not authorize changes to those interfaces. The existing `check-home-link-parents.sh` preflight also guards against deploying through stale parent symlinks into the checkout. That safety property is independent of the chosen linker.

Evidence: `modules/home/dev-tools.nix:1`, `:17`; `modules/home/tiling.nix`; `modules/home/dotfiles.nix:137`; `home/.config/git/config:50`; `home/.ssh/config:9`; `home/.config/tmux/tmux.conf:177`; `home/.config/zsh/.zshrc:630`; `docs/external-overlays.md`.

## 5. Nix-generated system configuration is a separate replacement concern

| Area | Generated/applied state | Repository owner |
| --- | --- | --- |
| Homebrew | Generated Brewfile pointed to by `HOMEBREW_BUNDLE_FILE`; selected taps, formulae, casks, trust and pin settings | `modules/darwin/homebrew.nix` |
| macOS preferences | Typed `system.defaults` and custom user preference values; screenshot directory and Spotlight sentinels | `modules/darwin/macos-defaults.nix` |
| System shell/session | `/etc` shell configuration, `EDITOR`, shell registration and Directory Services login-shell enforcement | `modules/darwin/core.nix` and nix-darwin |
| Daemons and services | Lix daemon/config/registry, garbage collection and optimization jobs, maxfiles LaunchAgent; tiling service restart/reload operations | `core.nix`, `tiling.nix`, framework modules |
| Packages and fonts | Nix profiles, application/font installation, font-version marker, certificates and terminfo links | `packages.nix`, Home Manager, nix-darwin |

Offline evaluation exposes 16 `environment.etc` entries, including shell files, Nix configuration, SSH fragments, certificates, sudo/PAM fragments, and profile/terminfo links. These are not 16 additional bespoke dotfile templates. Many are framework scaffolding whose purpose disappears with Nix, but currently active system paths still require an ownership handoff rather than deletion of the store first.

The nine empty home markers likewise represent directory scaffolding, not nine template use cases. Seven create this repo's extension seams; two are Home Manager cache/state markers.

Stow does not install packages, apply preferences, manage launchd, or replace these system-owned roots.

## 6. How chezmoi and Nix differ here

| Responsibility | Historical chezmoi snapshot | Current implementation |
| --- | --- | --- |
| Machine selection | Prompted `.machine`, `.chezmoidata/machines.toml`, templated ignore rules | `lib/machines.nix`, identity/capability gates, exported wrapper API |
| Variable substitution and branches | Go templates across 25 `.tmpl` files and `.chezmoiignore` | Mostly plain variants/native fragments; one custom Nix text flag; Python HOME/XDG substitution |
| Claude settings | `modify_settings.json.tmpl` rendered a shell/jq merge program | The same class of ownership-aware merge lives in `ai-stack.nix` activation |
| Global instructions | Shared Go-template instruction body and templated hook files | Plain source bodies, native imports, and two activation-time concatenations |
| MCP and skills | Templated post-apply scripts invoked Python generators | Nix activation invokes the Python generators |
| Secret deployment | Historical age-backed content and, during transition, 1Password rendering | Only the local 1Password renderer is owned by this repository; external work secrets are separately owned |
| System setup | Brewfile template and macOS/login-shell scripts | Nix module declarations and activation |

The template-engine change mostly moved selection and scheduling. MCP and skill generation were already separate tools during the chezmoi era. Reintroducing chezmoi would not eliminate those generators or their merge semantics.

Historical evidence: `git ls-tree -r 16f2060a3a00`; `.chezmoiignore`; `dot_claude/modify_settings.json.tmpl`; `dot_codex/AGENTS.md.tmpl`; `dot_codex/hooks.json.tmpl`; `dot_config/mise/config.toml.tmpl`; `dot_config/aerospace/aerospace.toml.tmpl`; `.chezmoiscripts/run_after_sync-{mcp,skills}.sh.tmpl` at that revision. Historical snapshots contain retired encryption configuration; none of it was executed or decrypted for this audit.

## 7. Findings that affect a replacement decision

1. **Stow alone is insufficient, but a new general template engine is not established as necessary.** The substantial transforms already exist in Python. Four user-output emitters are embedded in Nix activation. Only one custom nonempty home configuration is generated through `home.file.text`.
2. **Merge ownership is the requirement that must survive migration.** Claude settings, Claude MCP state, Codex TOML, and Codex hooks are not equivalent to wholesale generated files. Their application-owned data must survive regeneration. Instruction assemblies are different: their destinations are derived and may be replaced.
3. **The current trigger is larger than the required content operation.** Base/fragment edits are cheap, but the four activation emitters currently require activation to publish their results. MCP already has a targeted command. Secret rendering already has a standalone command. Their algorithms do not inherently require rebuilding packages or evaluating a flake.
4. **Changing the linker alone will not remove all slow work.** `just sync` includes rebuild plus side-channel provisioning. Side-channel sync installs/upgrades mise tools, refreshes sources, runs installers, and forces tool reinstalls. The fantasy-sports installer is called during activation and again during side-channel sync. These are verified operations, not benchmarked timing claims.
5. **Some documentation overstates or misidentifies generation.** `dotfiles.nix` comments still describe mise as generated, although `dev-tools.nix` links plain TOML. `mcp_sync/README.md` still describes chezmoi hooks and legacy mirror destinations. `docs/external-overlays.md` still describes the base SSH config as rendered and a global backup policy that current code no longer uses. Current source/evaluation should define the migration inventory.

Assessment: Stow plus retained generators is viable. Dotbot could express source mappings and invoke generators, but would not replace their algorithms. Chezmoi can render and schedule them, but would reintroduce the source/apply model for files that are already live-linked. The audit supports separating linking, content generation, secret rendering, and provisioning regardless of the selected linker. It does not support deleting generation or treating all generated files as immutable outputs.

## 8. Verification evidence

- Offline, no-update-lock-file Nix evaluation completed for `home.file`, activation dependency metadata, and `environment.etc`. No derivations were built or activated.
- The current MCP registry confirmed nine wholesale and two patched destinations.
- MCP destination, atomic-write, drift, and capture tests: **39 passed**.
- Claude foreign-hook preservation checks: **11 passed**.
- Claude settings key order, in-tool preferences, fragment precedence, malformed-fragment handling, and failed-normalization preservation checks: **passed**.
- The personal skills master/overlay resolve to 13 entries by source inspection and a read-only JSON merge. No skill deployment or fetch ran.
- Graph coverage was checked for the cited generator paths and relevant source scopes. Changed/unindexed source and relevant partial ranges were inspected directly. Counts came from current source/evaluation, not graph absence claims.
- The self-contained HTML was inspected in light/dark themes at 1280 and 390 CSS pixels. Layout, anchors, unique IDs, SVG node/edge titles, and the expanded destination list passed DOM checks. The browser console had no messages. An initial SVG validator incorrectly included accessibility metadata; the corrected node/edge selector passed without changing the artifact.
- Gitleaks scanned both report files and found no leaks. Whitespace checks produced no findings.

Commands used for fixture validation:

```bash
bash scripts/test-claude-settings-order.sh
bash scripts/test-claude-hooks-preservation.sh
uv run --offline --project mcp_sync --group dev pytest -q \
  mcp_sync/tests/test_destinations.py \
  mcp_sync/tests/test_atomic_writes.py \
  mcp_sync/tests/test_drift.py \
  mcp_sync/tests/test_capture.py
.claude/skills/mcp-sync-verify/scripts/print_target_paths.py --pretty
```

Several activation generators warn and continue on failure. A successful switch is therefore not proof that every generated output was refreshed. The MCP/skills hooks currently declare only `writeBoundary` as their activation dependency; the four settings/instruction emitters explicitly depend on `linkGeneration`. Their current execution order should not be confused with a portable regeneration contract.

The checks establish the audited source's current behavior. They do not establish live vault freshness, validate externally-owned generator implementations, or prove that every live destination is free of drift.
