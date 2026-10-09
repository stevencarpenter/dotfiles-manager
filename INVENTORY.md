# Initial ownership classification

**Keep safety and ownership rules in core; move role-specific content into selected modules; expose preference changes explicitly.** These are proposed classifications of the existing public configuration, not a completed manifest conversion.

Source baseline: `~/.dotfiles` at `1dbd12d3a563120a77aa3e27f68ad903a6d47d9e`. The generated-dotfiles audit provides evaluated counts: 126 Home Manager path entries, including 115 source links; 11 MCP destinations; four activation-written settings/instruction outputs; two secret-render targets. These scopes overlap and must not be added together. The counts are prior verified audit results, not a new evaluation performed for this draft.

First delivery targets only the personal M5 and work M3 (native Apple Silicon macOS). The Intel i9 remains independently owned by homelab and GNU Stow. Linux and homelab entries below are illustrative schema fixtures, not supported-target or migration promises. Future platforms constrain only the avoidance of OS-bound core semantics, mandatory GUI assumptions, and hard-coded model or prefix paths.

“Linked on every current machine” is evidence about the current configuration, not proof that a setting belongs on every future server. The public repository declares one host. The current private consumer has not been inspected; no actual work-machine configuration is inferred below.

## Core invariants

| Invariant | Proposed enforcement | Current evidence |
| --- | --- | --- |
| One declaration owner per path or managed field; foreign state survives | Resolver claims plus adapter-specific ownership | `modules/home/raw-dotfiles.nix`; `modules/home/ai-stack.nix`; `mcp_sync/src/mcp_sync/sync.py` |
| A destination cannot write through a stale parent link into source | Topology preflight and immediate pre-write checks | `scripts/check-home-link-parents.sh` |
| Required sources/overlays cannot disappear silently | Typed resolution error and blocked application | `mcp_sync` overlay and source validation; `scripts/verify-live-deployment.sh` |
| Secret outputs stay local, restrictive, and last-good on failure | Secret adapter; no generic capture or repository backup | `home/.local/bin/op-render`; `home/.config/op/adopt-policy.json` |
| External configuration extends public core without becoming a public dependency | Bundle import direction and protected catalogue identities | `docs/external-overlays.md`; `scripts/test-external-overlay-contract.sh` |

Git-source trust policy, local-fork ownership, and data-compatible upgrade rules are also safety contracts. Their mechanisms are invariant; their approved allowlists, package selections, and version policies remain explicit configuration. Tests should not fossilize current model names, editor choices, colors, or versions as safety properties.

## Exposed defaults and native extension points

| Current configuration | Proposed authority | Evidence and constraint |
| --- | --- | --- |
| Shared Git behavior and identity | Core native include topology; declared defaults for preferences; module-owned identity fragments | `home/.config/git/config`; neutral `~/.config/external-overlays/git/` namespace |
| SSH base and connection fragments | Core default source with explicit whole-file substitution; modules own distinct fragments | `dotfiles.nix` uses `lib.mkDefault` for `.ssh/config`; fragment-before-Host ordering must be retained or explicitly reviewed |
| tmux base, shell defaults, editor/theme preferences | Shared authored defaults with bounded native fragments or exposed whole-file slots | `home/.config/tmux/tmux.conf`; `home/.config/zsh/`; native sourcing order matters |
| Atuin and Worktrunk configuration | Shared default resources; explicit variant/source replacement | `dotfiles.nix` exposes both through `lib.mkDefault`; history and approvals must remain outside source |
| Agent style/model preferences and instruction fragments | Agents module owns managed fields and seeds absent preferences; permitted module contributions | `ai-stack.nix`; `dotfiles.nix`; models/effort are not unconditionally enforced over live preferences |

The fixture core protects its Git source but exposes its SSH source. This is deliberate: the existing SSH whole-file override must not be silently removed while demonstrating invariants. A source slot alone does not validate the effective native configuration; an adapter must prove any promised invariant across substituted sources and fragments.

AeroSpace, mise infrastructure fragments, MCP machine overlays, and skill machine overlays also have existing replacement seams. Preserve these as explicit defaults owned by their selected modules, rather than unrestricted global override precedence.

## Selected module content

| Candidate module area | Actual configuration it would own | Scope boundary |
| --- | --- | --- |
| Agents | MCP destinations, managed Claude settings, Codex hooks, Codex/Pi assembly, application plugins, skill distribution, output styles | Split platform-specific destinations as needed. Preserve wholesale versus co-owned outputs and seed-only preferences. |
| Development | mise development/infrastructure fragments, development CLI packages, Neovim/tooling preferences, worktree hooks | Tool requirements belong to modules. Do not maintain an independent capability truth alongside module selection. |
| Desktop macOS | AeroSpace, SketchyBar, Borders, terminal/UI configuration, display fonts, native reloads | Preserve accepted menu hover and workspace interaction. A selected config is not proof that a running process reloaded it. |
| Personal and homelab | Personal shell functions, Hippo, journal configuration, personal registry, optional sports tooling, Tailscale integration, declared 1Password templates | Device-specific journal data remains device-specific. Homelab reachability and account values do not become universal defaults. |
| Platform/system | Package owners, macOS preferences, login shell, launchd/systemd resources, fonts/certificates and privileged paths | Separate privileged adapters and explicit ownership handoffs. Existing Nix-generated state is not deleted by removing user links. |

A server composition can select only the core and its platform/server requirements. The Linux examples show the schema can express that composition; they are illustrative, not a supported-target or migration promise, and do not establish that the current macOS source files or service implementations support Linux.

The synthetic external work module demonstrates replacement, subtraction, hook contributions, and scoped Git identity. It is not a template copied from the real private repository, and it contains no claims about that machine's applications or security posture.

## Unmanaged application state and explicit exceptions

| State | Boundary |
| --- | --- |
| Atuin history, jj repository metadata, Nushell history, Worktrunk approvals/locks | Link only declared configuration files, not writable parent directories. |
| Pi/Codex sessions, authentication, trust, application caches and databases | Never infer source ownership from being beside a managed config file. No broad reverse capture. |
| Firstmate checkout, update queue, native lifecycle state | Native application owns these. The manager can own selected declarative launch/config files and issue explicit update requests. |
| z4h/plugin caches, shell-evaluation cache, compiled menu-hover helper, routing caches | Preserve each lifecycle owner. Derived caches are not source configuration and do not justify duplicate installers. |
| Tool-maintained declarative files such as Pi package settings and LazyVim lockfiles | Intentional exception: they are tracked source today. Preserve reviewable edits through supported links; do not reclassify every application-written file as unmanaged. |

Local generated secrets are a separate class: declaratively targeted but never checked into configuration source or treated as ordinary generated artifacts.

## Update ownership to preserve

- Homebrew activation currently disables auto-update, upgrade, and cleanup. Explicit maintenance owns Homebrew changes.
- Rolling mise upgrades during the existing sync workflow are intentional. The replacement's required `update` command must preserve rolling maintenance across all managed sources and modules. Its `sync` command applies fetched declarations without an unrelated global upgrade; this is an explicit command-scope distinction, not a frozen mise policy.
- Atuin currently belongs to Homebrew. A discovered Nix or alternate binary is a conflict to inspect, not a reason to install another copy.
- Firstmate coordinates its native updates and restarts. Requests, prerequisite setup, and observed completion remain separate results.
- Upstream skills and locally patched/vendored skills have different owners. A refresh must not overwrite a local fork or install a second owner at its target.

The version inventory must also include module Git sources, Nix/input or other package pins, uv/mise-managed CLIs, application plugins/extensions, and skill source/installer revisions wherever the selected composition manages them. The first implementation cannot limit updates to the root repository or only Homebrew/mise. Preserve intentionally held/local-fork policies and report them explicitly; mutable pins must have an owned update path.

Configuration adoption currently involves declaring an installation/update owner, copying selected configuration, choosing file versus directory links, registering a resource, clearing the exact collision, and verifying deployment. The first implementation must perform that promotion within `adopt`; the user must not reproduce the legacy manual Nix edit/rebuild procedure. `try` adds a recorded baseline, and `purge` adds confirmed, receipt-scoped cleanup without treating adjacent runtime state as source configuration.

Evidence: `Justfile`, `modules/darwin/homebrew.nix`, `scripts/sync-side-channels.sh`, `scripts/update-inputs.sh`, `modules/home/dotfiles.nix`, and `docs/adopting-a-config.md`. No update, secret render, service operation, or live migration was run for this draft.
