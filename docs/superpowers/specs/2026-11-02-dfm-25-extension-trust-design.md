# DFM-25 C04 and C05: extension source trust and owned pin edits

Status: approved by the owner on 2026-11-02. Kaneo: DFM-25 (C04, C05). Inputs: the code at main 97c37b8 and `~/.dotfiles` at 79e20c3.

## Intent

A pull of the dotfiles repository can add a plugin marketplace, a skill source or an npm package to a tool's configuration. The next `dfm sync` writes that configuration, and the next updater run fetches and executes code from the new source. Neither step needs the owner's explicit trust today. A new marketplace in `~/.dotfiles` therefore becomes code that runs on the machine.

`dfm sync` already has this rule for its own sources. An origin must have a grant in `state/grants.json` before it is fetched, and `--trust-origin` writes the grant and then stops for review (docs/sync.md, step 1 and step 4). This design applies the same rule to the external sources that dfm writes into tool configuration and that its updaters fetch.

C05 covers the other direction. An update must not silently change a tracked file in a source checkout. A pin or lock change that dfm makes must land in the repository that owns the file, with a record of what made it.

## Goals

- **G1.** No dfm operation adds an external source identity to a tool's configuration unless that identity has a grant on this machine. "Adds" means that the planned content names the identity and the live file does not.
- **G2.** No updater that fetches extension sources runs while one of those sources has no grant. At 97c37b8 these are `skills` and the `agents` updater of DFM-25.C09. `claude update` and `codex update` replace only the tool binary (`claude update --help`: "Check for updates and install if available"). Claude fetches its marketplaces and plugins itself (`claude plugin marketplace update`, or the app's auto-update), so for Claude the control point is G1: dfm writing a marketplace into its configuration.
- **G3.** A grant is explicit and per machine. It is never inferred from a pull, from a fetch of metadata, or from the source being already present.
- **G4.** dfm never runs code from an extension source itself. Only the owning tool's own updater (the fixed set, docs/upgrade.md) fetches and installs, and only from granted sources.
- **G5.** No updater leaves a tracked file changed in a source checkout without the owner knowing. A tracked change that an updater makes is reported and fails that updater's row. It is never reverted, committed or pushed.
- **G6.** A declared updater or source that cannot run (missing credential, read-only checkout, an unsupported managed version) appears as a `blocked` row with its cause. It is never dropped from the update set.

## Non-goals

- Pinning plugin, skill or extension versions. The owner's model is rolling by default (owner tool history). Grants control where code comes from, not which version runs.
- Signature or checksum verification of plugin contents.
- Moving the side-channel installers (`scripts/sync-side-channels.sh`: codex marketplace add, `skills add`, fantasy-sports, token-auditor) into dfm. That is C01 inventory work. This design gives each of them a grant check once they move.
- Claude's own plugin auto-update toggle. It is the application's setting, and dfm does not change it.
- C03 (destination claims) and the C06 test matrix beyond the cases named here.

## Anti-goals

- A trust prompt on every sync. A grant is written once per identity per machine.
- A partial updater run. An updater fetches from all of its sources at once, so dfm cannot exclude one ungranted source from it. The whole row is blocked instead.
- A second trust store. Grants stay in `state/grants.json`.

## Source identity

One identity string per source, compared by exact string equality, as `Grants::trusts` compares origins today (`crates/model/src/lock.rs`).

| Source kind | Example in config | Identity |
| --- | --- | --- |
| Git repository (marketplace `{"source": "github", "repo": "o/r"}` or `{"source": "git", "url": ...}`, skill git source, agent registry remote) | `anthropics/claude-plugins-official` | the canonical origin of `check_origin`: `https://github.com/anthropics/claude-plugins-official` |
| Marketplace manifest URL (`{"source": "url", "url": ...}`) | `https://example.com/marketplace.json` | the URL in the canonical form of `check_origin` (`https` only) |
| Git SSH remote | `git@github.com:stevencarpenter/agents.git` | `ssh://git@github.com/stevencarpenter/agents` |
| npm package (OpenCode plugin, an `npx` installer) | `@dietrichgebert/ponytail` | `npm:@dietrichgebert/ponytail` |

- A GitHub shorthand `o/r` maps to `https://github.com/o/r`. The SCP form `git@host:path.git` maps to `ssh://git@host/path`. Then `check_origin` must accept the result. A value that does not map is `E_SOURCE_UNTRUSTED`, and it is never fetched.
- The `https` and `ssh` forms of one repository are two identities. This matches the existing origin rule.
- `npm:` identities name the package only, on the default registry. A config that names another registry is `E_SOURCE_UNTRUSTED`.
- A local marketplace (`{"source": "directory"}` or `{"source": "file"}`) has no identity. In planned content it is `E_SOURCE_UNTRUSTED` with the fix text "a managed marketplace must be a git or URL source". No live marketplace on the M5 uses one (all five are `github` at 2026-11-02).
- Sources inside a granted bundle (for example `skills/v1` entries whose `source` is a path in the bundle) have no identity of their own. The bundle's origin grant covers them.

## Grants

- `state/grants.json` keeps its `origins` list for git identities. `npm:` identities go in a second list (`packages`), with the same fields (`grant_id`, `granted_at`). A grants file without `packages` stays valid. The schema change follows docs/state.md for a document change.
- `dfm sync --trust-origin <identity>` accepts both forms. The rules from docs/sync.md step 1 and step 4 apply unchanged. The grant is written under the writer lock between a `grant-origin` intent and its `done` entry, before any fetch. A run with `--trust-origin` and without `--yes-plan` stops with `E_APPROVAL_REQUIRED`, so the owner reads the plan that the new source brings first.
- No other command writes a grant. A grant is never written because a source is already in a live file.
- **A marketplace grant covers the plugins it lists**, also plugins whose source is another repository or an npm package (owner decision, 2026-11-02). The owner trusts the marketplace's curation. dfm does not read Claude's cached marketplace manifests.
- A grant authorizes two things for that identity: the owning tool fetches it, and the owning tool installs and runs what it fetched. A fetch of metadata (`git ls-remote`, `npm view`) is not a grant. In this design, dfm makes no network call for an ungranted identity at all.

## Where the check runs

### Plan time: configuration that names sources

An adapter that writes configuration that can name external sources reports the identities in the content that it plans. The first such adapter is `claude-settings/v1`. Its managed `extraKnownMarketplaces` entries are identities. `enabledPlugins` keys (`name@marketplace`) are not identities: they select a plugin from a marketplace that is either built into Claude, or in the live file, or declared next to them. An adapter added later (an OpenCode or Codex config merge) uses the same hook.

- A planned operation whose content adds an identity (named in the planned content, absent from the live file) without a grant is `blocked` with `E_SOURCE_UNTRUSTED`. The message names the identity and the command to run: `dfm sync --trust-origin <identity>`.
- A blocked configuration operation follows the existing sync rules for a blocked configuration part (docs/sync.md: the run is refused before publication, and a blocked later part is warned). No new rule is added.
- An identity that is already in the live file is not blocked, because writing it adds nothing: the owner's earlier action put it there. This also keeps an operation that is `current` from becoming blocked, so the first sync after this ships does not refuse the configuration part.
- A foreign marketplace that the user added by hand to the live file is the application's state. The merge keeps foreign keys (docs/adapters.md), and dfm does not grant or block it.

### Updater time: what the tool fetches

Before an updater runs, dfm reads the identities that the tool will fetch from, read-only, from the tool's live state:

| Updater | Identities read from |
| --- | --- |
| `skills` | the `skills` CLI's own source record, and `npm:skills` for the installer that `npx` runs |
| `agents` (DFM-25.C09, built in its own PR first) | the `origin` remote of the copy that the updater uses |
| `claude`, `codex`, `opencode`, `brew` | none: they update the tool binary or the packages from their owner, not extension sources |

- Each identity without a grant makes the row `blocked` with `E_SOURCE_UNTRUSTED`, and that updater runs no command. The note lists every ungranted identity and the one `--trust-origin` command for each.
- If the source list cannot be read (the file is missing, unreadable or in an unknown format), the row is `blocked` with that cause. It is not `current`, and it is not run without the check.
- The check runs in the review listing (docs/upgrade.md, "In sync", item 1) and again under the writer lock just before the updater runs, because the tool state can change in between.

### The code execution boundary

- dfm runs only the fixed updater commands and scripts from the bound checkout (for example `scripts/install-agent-registry.sh`). The bound checkout is the granted root source.
- The agent registry install runs the registry's own code (`uv run ... agent_registry.cli install`). The registry origin's grant covers this. Without the grant, the `agents` row is blocked before `git pull`.

## C05: tracked files and pin edits

### Inventory at 79e20c3

No updater in the fixed set writes a tracked file in a source checkout today:

- `~/.pi/agent/extensions/package-lock.json` is a link into `~/.dotfiles`, and the owner updates it by hand with `npm`.
- `versions/token-auditor` and the `skills add` pin in `scripts/sync-side-channels.sh` are edited by hand.
- Nested source pins in `dotfiles-manager.lock` are never written by `sync` (docs/sync.md, "What the lock declares").

So no pin edit is eligible yet. The design adds a guard now and defines the rule for the first updater that is allowed to change a pin.

### Guard

- Before the updaters run, dfm records `git status --porcelain=v1 -z` for the bound checkout and each nested source checkout. After each updater, it records it again.
- A tracked file that the updater changed (modified, deleted or type-changed) makes that row `failed` with `E_CONFLICT`. The note names the file and the updater. A file that resolves through a dfm link counts as the file in the checkout.
- The change is left in place for the owner. dfm does not revert, stage, commit or push it.
- The next `dfm sync` refuses the dirty checkout through its existing rule (a local change to a file the update changes, docs/sync.md step 2), so the change cannot be lost silently.

### Rule for an eligible pin edit

An updater may be allowed to change a pin only through a change to this design. When one is allowed:

- It writes the change only in the checkout of the repository that owns the file, and only when that checkout is clean and on its tracked ref.
- It records the change as one local commit on the current branch, with trailers `Dfm-Updater: <name>`, `Dfm-From: <old>`, `Dfm-To: <new>` and `Dfm-Receipt: <receipt id>`. It does not push. The existing rule "a checkout ahead of its ref is current" (docs/sync.md step 2) keeps the next sync working, and the owner pushes when ready.
- A dirty checkout, a detached HEAD, a read-only checkout or a failed commit makes the row `blocked` with its cause, and nothing is written.

### Blocked instead of shrunk

- A declared updater that cannot run because of a missing credential (for example no SSH key for the registry remote), a read-only checkout or a managed version that the adapter does not support is `blocked` with that cause. It is not `current`, it is not left out of the listing, and it counts in the `upgrade:` summary and the warnings (docs/upgrade.md, "In sync", item 8).

## Decided in this design (my calls, open to the owner)

1. **Grants per repository, not per owner.** No `github.com/<owner>/*` form. The owner's own sources are few (dotfiles, agents, fantasy-sports, token-auditor). One grant each keeps the rule exact.
2. **No automatic grant on upgrade.** The first sync after this ships blocks the `skills` and `agents` rows until the owner grants their identities. The configuration part is not blocked, because its identities are already in the live file (G1). The preview prints one `--trust-origin` line per identity. This is one-time work per machine.
3. **`npx skills update` stays rolling**, but it needs a grant for `npm:skills`.
4. **One flag.** `--trust-origin` takes npm identities too. No new command.
5. **The tracked-file guard fails the row and does not refuse the run.** The other updaters still run.

## Left to the builder

- How an adapter reports its identities (a method on the adapter, or a function next to it) and where the plan-time check sits in the planner.
- The exact file and format of the `skills` CLI's source record. Read each one on the M5 (read only) and document it in docs/upgrade.md. A format that cannot be read is `blocked`, as specified above.
- The test layout. Required cases: a new marketplace in planned content blocks the operation; a granted one passes; a marketplace already in the live file does not block a `current` operation; a hand-added foreign marketplace in the live file is kept by the merge; a local marketplace in planned content is refused; an ungranted registry remote blocks `agents` before any git command; an unreadable source list blocks; an updater that changes a tracked file fails its row with the file named; an `npm:` grant round-trips through `grants.json`; a grants file without `packages` still parses.
- Documentation updates in docs/sync.md (the trust rule and the flag), docs/upgrade.md (statuses and notes), docs/state.md (the `packages` list) and docs/adapters.md (the `claude-settings/v1` identities).

## Delivery

Two slices, each its own PR:

1. Identities, grants (`packages` list and the `npm:` form) and the updater-time check (G2, G3, G6), plus the tracked-file guard (G5). It builds on the `agents` updater of DFM-25.C09, which merges first.
2. The plan-time check in `claude-settings/v1` (G1).

After the M5 self-updates to slice 1, the owner runs `dfm sync --preview`, then grants each identity it lists with `dfm sync --trust-origin`. Until then, the affected updater rows are `blocked`, and the rest of the sync runs.
