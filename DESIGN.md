# Machine configuration contract

**The manager resolves one machine profile into an owned configuration and an explicit change plan. A mandatory core defines shared invariants and overridable defaults; selected modules contribute through declared extension points.**

Status: requirements contract for the `dfm` manager. The owner approved the requirements in section 1 and the ownership boundary in section 2. Sections 3 to 9 specify the design. The manager implements part of it: section 7 maps each proposed command to the shipped command, and section 11 lists what is built and what stays open.

## 1. Agreed requirements

1. Each machine has exactly one machine-specific profile. There is no interactive profile-switching workflow and no inference of a different profile from current applications or network location.
2. A default core is authoritative across machines. Core invariants cannot be overridden. Core defaults may be overridden only through explicitly exposed points, with provenance.
3. Role-specific configuration belongs in modules. Work, personal-development, homelab, and server modules can carry their own application plugins, extensions, packages, and settings.
4. Installation accounts for the machine's platform, existing files, package owners, application state, and service lifecycle. Existing state informs a safe transition; it does not silently become desired configuration.
5. A full replacement manager is in scope. Incremental implementation and migration do not restrict the eventual product to a task-runner wrapper.
6. First delivery requires only two machines: the personal M5 and the work M3, both native Apple Silicon macOS. The earlier three-machine requirement is superseded. All four mandatory workflows and full replacement still apply to these two machines.
7. The Intel i9 remains independently owned by the homelab repository and GNU Stow. It is not a first-delivery target, and this design makes no migration promise for it.
8. Linux and desktop Macs (Mac Studio, Mac Mini) are not targets. The design only avoids OS-bound core semantics, mandatory GUI assumptions, and hard-coded model or prefix paths. This adds no platform implementation, portability framework, or delivery gate.

### Mandatory first-implementation workflows

These are user-required capabilities of the **very first implementation**, not optional additions after a read-only release. The command names below are the proposed names. Section 7 shows where the shipped commands differ.

| Workflow | Proposed command | Required result |
| --- | --- | --- |
| Update all managed versions | `dfm update` | Discover, preview, approve, and apply eligible version updates across every managed source in the bound profile's complete module graph, including external modules. |
| Pull and apply configuration | `dfm sync` | Fetch the latest dotfiles from the configured GitHub tracking ref, resolve the required module revisions, and apply the resulting configuration in one invocation. |
| Try, then keep or remove a tool | `dfm try <tool>`; then `adopt <trial> --module <id>` or `purge <trial>` | Start a tracked trial without hand-authoring a module, preserve its baseline, and either promote its tuned configuration and installation into a module or remove its attributable trial changes. |
| Diagnose the machine | `dfm doctor` | Produce a read-only diagnostic table of tools/resources, health, versions, executable/config locations, ownership, and evidence-backed findings with suggested actions. |

For each mutating workflow, one command includes discovery, planning, approval, execution, and verification. It must not require a separate manual pull, per-provider update loop, or plan/apply invocation. Authentication, conflict resolution, and confirmation of destructive effects may still require interaction within that workflow. An internal read-only resolver is an implementation step, not fulfillment of this scope.

The implementation belongs in this repository. Public shared configuration remains a separate consumer, initially the existing dotfiles repository. Private module repositories depend on that public configuration, never the reverse.

## 2. Authority and ownership

There are three declaration authorities:

- **Core:** mandatory resources and protected topology, plus explicitly exposed defaults and extension maps.
- **Module:** additional resources, permitted changes to another resource, and extension entries owned by that module.
- **Machine profile:** selection and explicit machine-specific changes under the same permission rules as modules.

The profile is not an unrestricted override tier. A dependency does not confer authority to override its parent. File order, module order, and dependency order never decide a conflict.

Separate declaration ownership from application ownership. Several modules can contribute to one manager-owned document through its owner. The manager publishes that document once. For co-owned documents, the adapter preserves fields and handlers owned by the application or other installers.

### File classes

| Class | Authoritative input | Publication contract |
| --- | --- | --- |
| Authored file | Ordinary source file | Explicit link; detect replaced links and unsafe parents. Directory links are opt-in. |
| Generated file | Declared inputs and adapter | Validate, then publish atomically. Live edits are reported as drift. |
| Co-owned document | Declared managed fields plus live foreign state | Adapter-specific merge, not blanket replacement or generic array merging. |
| Secret output | Declared references/template plus authenticated provider | Redacted planning, restrictive permissions, last-good preservation. |
| Runtime state | Owning application | Not managed as configuration or imported into source control. |

A path or field is owned only when declared. Nothing implicitly owns HOME, a whole application directory, or all keys in an application document.

## 3. Bundle, module, and profile documents

TOML is the human-authored format. It permits comments and maps to a bounded data model. Schema version 1 accepts known keys only. There is no embedded expression language or shell interpolation engine.

### Bundle

A root `bundle.toml` supplies one `core` module manifest and a catalogue of other module manifests. An external bundle may `extends` one bundle and add catalogue entries. It cannot replace the core, redefine an inherited module ID, or change an inherited module's source. Recursive extension cycles are errors.

The catalogue does not enable modules. The profile selects them. Examples use explicit local paths relative to their declaring documents. Production source acquisition is an explicit phase inside `sync` and `update`, not a separate command the user must run first. Standalone read-only resolution still requires trusted, materialized, locked sources. Missing source material is an error, not permission for `plan` to fetch it.

A source lock records the content identity of each trusted bundle/module and adapter version. Module code provenance is independent of package policies such as rolling releases. Local editable source changes invalidate an old plan rather than requiring a network refresh.

### Module

A module declares `schema`, a unique `id`, supported `platforms`, `requires`, resources, and optional overrides/contributions. IDs use lowercase letters, digits, and internal hyphens; they start with a letter. The core's ID is `core`.

`requires` selects prerequisite modules. Core is implicitly required and cannot depend on optional modules. Unknown modules, duplicate selection, unsupported platforms, or dependency cycles fail resolution. For stable presentation, core is first and remaining ready modules are ordered lexically.

Module dependencies determine availability, not execution barriers. For example, a work module can require the agents module while supplying an instruction fragment that the agents assembly reads. The resource input/output graph, not whole-module execution order, puts that fragment before its reader.

### Profile

A profile declares `schema`, an individual machine `id`, `bundle`, expected `platform` and `architecture`, selected `modules`, and optional `overrides`. It cannot declare another core, suppress the core, or introduce arbitrary resource kinds.

The machine is bound explicitly to one profile at `~/.config/dotfiles-manager/machine.toml`. Binding records the canonical profile path, the checkout root, and the machine identity; it is not inferred from hostname alone. A platform mismatch fails before mutation. A deliberate rebinding is an ownership migration, not `apply --profile other`.

Multiple fixture profiles in this repository are alternative machine examples. A deployed machine has one binding and no stack of active profiles.

## 4. Resource format

Each resource declares:

- `id`, `kind`, and a versioned `adapter` identifier.
- `target` for a single filesystem output, when applicable.
- A typed `value` tree interpreted by the adapter.
- Optional `policy.defaults` and `policy.extensions` lists.

The global resource ID is `<module-id>.<resource-id>`. A resource may not redefine another resource ID. Profiles do not own resources; they can supply permitted overrides with their profile ID as provenance.

`target` is HOME-relative. Absolute targets, empty components, `.` or `..` components, and NUL bytes are rejected. System paths require a distinct privileged adapter with explicit scope; a HOME adapter never escalates.

Resource inputs such as `value.source`, `value.body`, and `value.template` are relative to the manifest that supplies that value. The resolver retains that origin after an override. The adapter checks that each source remains within the supplying module's trusted root, including through source symlinks. Resource output paths and source paths are different types.

The manager does not interpolate arbitrary strings. Adapters may support documented tokens, such as HOME expansion in a native hook command, and must serialize values safely. Manifests cannot ask the generic engine to execute shell snippets.

### Initial adapter boundaries

| Kind | Example adapter | Required behavior |
| --- | --- | --- |
| `link` | `fs-link/v1` | Claim the exact leaf. Detect parent links and file/directory transitions. Never force over unknown content. |
| `render` | `git-config/v1` | Serialize a typed native document into a wholly owned file. |
| `merge` | `claude-settings/v1` | Preserve foreign settings/handlers, retire only owned handlers, and seed preferences only when absent. |
| `assemble` | `text-fragments/v1` | Body plus lexically ordered fragments, validated before atomic publication. |
| `distribution` | `skills/v1` | Expand named entries into exact destination claims; preserve unmanaged skills and enforce source trust. |
| `secret` | `op-template/v1` | Authenticate only during explicit secret application; never reveal values in plans or receipts. |
| `package` | `homebrew/v1`, `mise/v1`, `apt/v1` | One owner for a package; installation, update policy, and observed version remain distinct. |
| `application` | `firstmate-native/v1` | Delegate lifecycle to the native updater and distinguish queued requests from completed updates. |

These identifiers specify contracts and are not an exhaustive first-implementation provider list. The registry of shipped adapters is `crates/cli/src/adapters.rs`; [docs/adapters.md](docs/adapters.md) and the per-feature documents describe each one. The first implementation must cover every managed version source in its supported target profiles, including module Git sources, package/input pins, runtimes, application plugins/extensions, and skills. An unsupported managed source is a coverage failure, not a silent exclusion from `update`.

Reuse existing domain algorithms behind these boundaries where their behavior fits. Application plugin configuration is ordinary module content, not evidence that a manager-plugin marketplace is needed.

Distribution adapters must enumerate leaf outputs before apply. Two independent resources claiming the same file fail. A directory link conflicts with every independently claimed descendant. Claims are checked against the target filesystem's case and path-alias behavior, not merely raw string equality.

## 5. Overrides and extension entries

Policy paths are RFC 6901 JSON Pointers into `value`, even though the manifest is TOML. The empty root pointer, array-element addressing, wildcard paths, and overlapping policy paths are not allowed. Arrays are atomic values. Named collections use maps, not position-dependent array edits.

All values are protected unless the owner lists a path in `policy.defaults` or `policy.extensions`.

### Override a default

An override names `resource`, `path`, `value`, and a nonempty `reason`.

1. The referenced resource must belong to the selected composition.
2. The path must exactly match an exposed default and exist in the owner's value.
3. The replacement must satisfy the owner's type and adapter schema. Boolean and integer types are distinct. Overrides cannot change schema, adapter, target, ownership policy, or resource identity.
4. At most one override may address a resource/path pair, even if two proposed values are equal. The base default is not an override. An explicit profile override competes under the same rule as a module override.
5. An override replaces that value. It does not recursively acquire permission over neighboring keys.

Disabling an optional entry is an explicit boolean default such as `/entries/sample-audit/enabled`. Protected entries cannot be disabled. Role-specific entries should normally be absent unless their owning module is selected, rather than enabled globally and disabled elsewhere.

### Extend a named map

A contribution names `resource`, an exposed map `path`, a `key`, a `value`, and a nonempty `reason`. The key must start with the contributing module ID followed by a dot. The owner validates the entry schema. An existing key cannot be replaced through contribution, and two contributors cannot claim one key.

A contributed object belongs to its contributor. Other modules cannot mutate it. An owner can expose a replaceable complete value as a default when a true substitution is required; generic deletion or last-writer-wins merging is not supported.

Effective provenance records the declaring owner, original value, permitted change, changing module/profile, reason, source file, and content identity. Secret-bearing data is represented by references and redacted metadata, never by secret values or their hashes.

### Native includes

A module can also own a distinct native fragment, such as Git's conditional identity include. This is an additional resource, but its values may override defaults according to the application's rules. The owner of the base file defines the include order and scope. Native precedence must be visible in `explain`; it is not a loophole for undeclared shared-file mutation.

Core can protect its include topology. Protecting an effective native setting against every possible fragment requires an adapter capable of validating the resulting native configuration. Otherwise the manager must report that the invariant is not enforceable by that adapter, not claim that a protected manifest field secures it.

## 6. Resolve, inspect, plan, apply, verify

1. **Resolve:** load trusted local sources, bind one core, validate references/platforms, close dependencies, and apply permitted changes. No target writes, network, vault calls, provider installation, or manifest-supplied code execution.
2. **Inspect:** read scoped local facts: files, parent topology, installed ownership, and supported service state. An adapter must document any native read command and prevent its auto-update/network side effects. Unknown facts remain unknown.
3. **Plan:** produce typed operations from the resolved model and observations. Include target/field ownership, reasons, old-state fingerprints, source identities, dependencies, privileges, and redacted effects. Planning is deterministic for identical inputs and observations. Display ordering never establishes override precedence.
4. **Apply:** verify the reviewed plan's source and observation preconditions, then execute approved operations in dependency order. Serialize this manager's own writers. Reject stale plans or changed targets rather than overwrite concurrent edits.
5. **Verify:** test each operation's declared postcondition. A correct file, an installed executable, and a reloaded consumer are separate results.

Adapters declare their input paths and exact output claims. The planner adds producer-to-consumer edges, including selected fragment producers. It also captures existing foreign fragment inputs. Cycles or newly appearing/disappearing inputs invalidate the plan. Explicit native lifecycle prerequisites add operation edges without imposing whole-module barriers.

Each ordinary file publication uses a same-directory temporary file and atomic rename after validation. Recheck parent topology and live state immediately before writing. A manager lock cannot stop applications from writing; co-owned adapters must re-read/merge safely or require quiescence where reliable conflict detection is unavailable. Do not promise a filesystem-wide compare-and-swap or a transaction across external tools.

On failure, dependent operations are blocked. Stop mutation by default and report applied, failed, blocked, and not-started work. Do not hide a required failure behind a warning or a successful system switch.

## 7. Commands and state

The executable is `dfm` (owner decision 2026-10-20, DFM-67). The "Shipped" column records the command at the current revision. `dfm --help` is the authoritative usage text.

| Operation | Contract | Shipped |
| --- | --- | --- |
| `plan` | Read-only resolution/inspection and an inspectable proposed change set. No automatic fetching or authentication. | `dfm plan` ([docs/planner.md](docs/planner.md)). |
| `explain <resource-or-path>` | Show declaration ownership, permitted overrides, provenance, native precedence, and runtime-verification status. | `dfm explain <resource-or-target>` ([docs/resolver.md](docs/resolver.md)). |
| `doctor [tool-or-resource]` | Read-only diagnostic table and findings, including versions, executable/config locations, ownership, drift, and probe failures. Support full-detail selection and `--json`. | `dfm doctor [--json]`: binding, state and owned files only. No tool, version or PATH probe and no per-component selection ([docs/doctor.md](docs/doctor.md)). |
| `status` | Compact view of the same inspection results used by `doctor`, comparing desired, last-applied, current, and consumer-loaded state where observable. | `dfm status [--json]`, the same rows as `doctor`. |
| `apply <plan>` | Apply an explicitly reviewed plan. A local configuration plan cannot contain implicit upgrades or secret refreshes. | `dfm apply`: plans again and applies only with `--yes-plan <digest>` of that plan ([docs/apply.md](docs/apply.md)). |
| `update` | One-command source refresh and version maintenance across the complete selected composition, followed by applying and verifying the approved result. | Partly built (DFM-31). `dfm sync` runs the updaters the profile declares from a fixed set: `brew`, `claude`, `codex`, `opencode`, `skills` ([docs/upgrade.md](docs/upgrade.md)). `dfm packages --update` advances rolling mise and uv tools ([docs/toolchains.md](docs/toolchains.md)). `dfm self-update` rebuilds the manager (DFM-72, [docs/bootstrap.md](docs/bootstrap.md)). |
| `sync` | One-command GitHub pull-and-apply of published configuration and its required module revisions, without independently bumping every tool version. | `dfm sync`: configuration, packages (missing installs), secrets and services in one run (DFM-63), without a digest (DFM-76, [docs/sync.md](docs/sync.md)), then the profile's declared updaters (DFM-31, [docs/upgrade.md](docs/upgrade.md)). mise and uv tool bumps stay with `packages --update`. |
| `try <tool>` | Discover its installation owner and configuration candidates, record a baseline, and start a tracked trial. | `dfm trial <package>` (DFM-32, [docs/trial.md](docs/trial.md)). |
| `adopt <trial-or-path> --module <id>` | Review and promote selected configuration plus installation/update declarations into the chosen module, then verify the ownership handoff. | Not built (DFM-33). `--allow-adopt` on `apply` and `secrets` is a different operation: it records ownership of an existing output. |
| `purge <trial>` | Preview and confirm removal of trial-owned installation/configuration/state and safe restoration of the recorded baseline. | Not built (DFM-34). |
| `secrets` | Explicitly authenticate/render declared secrets. Preserve last-good outputs on failure. | `dfm secrets` ([docs/secrets.md](docs/secrets.md)). |

The shipped CLI also has `bind`, `binding`, `state`, `packages`, `services`, `self-update` and `self-check`. [README.md](README.md) lists the exit codes and links the document for each command.

Only configuration work belongs in a normal config plan. The mutating high-level commands compose source acquisition, configuration, provisioning, updates, and necessary reload scopes internally, with one combined preview and approval rather than separate user invocations. Secret authentication/rendering remains an explicit opt-in scope; a missing required secret blocks dependent work instead of being silently ignored.

### Update every managed version

`update` starts with source refresh, resolves the prospective composition, and enumerates versioned resources from the core, selected modules, and their dependencies. Modules in separate public or private repositories participate through the same contract. It does not manage unrelated installed tools, other machines, or arbitrary repositories found on disk. Refreshing a shared repository may bring upstream files for unselected modules, but does not authorize installing or updating their resources on this machine.

1. Each participant reports its owner, source, current version, eligible candidate, policy, required declaration/lock edits, prerequisites, and verification method. Discover candidates before mutating live installations. Missing credentials or unsupported sources block preparation rather than silently shrinking coverage.
2. Cover repository/module revisions, package and input pins, runtimes/CLI tools, application plugins/extensions, skills, and native applications. Preserve existing native update owners instead of installing a competing copy. If the manager itself is a managed package, replace it only after the running operation has recorded its result.
3. “Latest” means the latest eligible version under the declared release channel, compatibility range, and soak policy. An exact resolved pin makes application reproducible; it is not automatically a permanent hold. `update` must be able to advance managed pins and lockfiles. An intentional hold is separately declared and reported with its reason.
4. Present one combined diff and approval. Store changed declarations in their owning source, never private module details in the public core. Pin changes remain reviewable local edits; no automatic commit or push. Read-only sources must expose a supported update path or block the operation, not be rewritten elsewhere without provenance.
5. Apply in dependency order and verify actual outcomes. Account for every participant as updated, already current, explicitly held, pending, failed, or blocked. A queued native update is not completion. Wait through its supported completion protocol or report pending without claiming full success. Resume from receipts after failure; do not claim a transaction or universal rollback across providers.

**Superseded in part (owner, 2026-10-20).** The default policy is rolling: each participant moves to the newest version its provider resolves, with no soak period and no channel requirement. A hold stays an explicit declaration with a reason. This replaces the "soak policy" in item 3, section 8 and section 9 (DFM-31.C03, [docs/roadmap-review.md](docs/roadmap-review.md)).

This extends the existing unified `just update` intent to **all managed sources and modules**, not only a fixed list of package managers. Existing version policies remain deliberate; the CLI cannot report everything updated while omitting a provider or external module.

### Pull the latest configuration and apply it

`sync` fetches the configured GitHub tracking ref and the module revisions required by the fetched configuration, validates the resulting single-profile composition, shows the change set, and applies it in the same invocation. Tracking sources advance to the latest declared ref; exact module references follow the published declaration. Independently advancing such pins belongs to `update`.

Fetch and validate candidate revisions in staging before changing a linked source checkout: a checkout fast-forward can itself change live configuration through existing symlinks. Check all affected sources, credentials, ownership, and required adapters before publishing source changes. New source origins require explicit trust; fetching a manifest is not permission to execute repository code.

Preserve local commits and edits. Refuse divergent history, overwritten untracked files, and conflicts with local changes; never auto-reset, clean, rebase, or stash the user's work. Unrelated edits must be preserved. Recheck source and destination preconditions before publication. A failed fetch or invalid module cannot fall back to stale or core-only configuration and report successful sync.

Apply the fetched declared versions and install missing prerequisites when required. Do not turn a configuration sync into an unsolicited upgrade of every floating dependency. Required generation and safe reloads belong to the operation; disruptive restarts need explicit approval. Record source revisions, changed outputs, and observed consumer state, including partial failures. Neither `sync` nor `update` commits or pushes source changes implicitly.

### Trial, adopt, or purge

`try <tool>` must not require a hand-written module first. Use the selected installation provider to discover the tool and suggest configuration paths; ask only for ambiguous provider/path choices. Unsupported tools can use explicit configuration paths with a supported package provider. Show any cleanup coverage that cannot be established before proceeding.

Record whether the tool, dependencies, services, and scoped paths already existed, together with their owners and recoverable baseline. Prefer application-native isolated configuration when supported. Otherwise use narrowly scoped native locations, with permission before changing existing configuration. A trial remains local state attached to the same bound profile, not a second profile or a bypass of core authority. It survives shell exits and remains visible in `status`; ordinary `sync`/`update` preserve its ownership and baseline and do not adopt, upgrade, or remove it implicitly.

- **Adopt:** discover and review the tuned configuration, choose its destination module, scan for secrets, and generate the minimum installation/update/resource declarations and source files. Prefer file-level adoption when runtime state shares a directory. Preserve exact selected configuration content, validate the prospective composition, apply it, and verify that a subsequent sync reproduces the configuration without a duplicate owner. Do not capture credentials, sessions, caches, databases, or foreign fields. Private configuration stays in a private module. Keep the trial recoverable until adoption verifies; do not commit or push automatically.
- **Purge:** show exact proposed removals/restorations and require confirmation. Uninstall only trial-owned installations no longer required by another module or trial. Remove identified trial-created configuration and any explicitly approved trial state; restore a pre-existing file only when the current state still matches the recorded trial-owned result. Concurrent/unattributed edits, unsafe parents, or missing receipts block the affected cleanup. Preserve pre-existing tools, shared dependencies, credentials, and unrelated processes/data. Retained items and reasons appear in the result; never use broad package autoremove or recursive HOME deletion.

The manager is not an OS sandbox and cannot promise to undo arbitrary effects of an uninstrumented application. Adapters declare their installation and filesystem scope; discovery outside it produces candidates for review, not inferred deletion authority. User-added configuration paths make an unfamiliar tool usable without writing a custom adapter, but do not establish an earlier baseline retroactively. A completed purge needs verified cleanup or an explicit account of retained/unattributed effects, not an unsupported claim that the machine is pristine.

### Doctor: read-only machine diagnostics

`doctor` is a required diagnostic command, not an installer, updater, repair command, or alias for “is this executable on PATH?”. Reuse the resolver, inspection adapters, ownership records, and verification evidence used by plan/apply. `status` is a compact presentation of the same observations, not a second health-check implementation.

By default, report the bound profile's complete selected module graph and active trials. Include missing tools and config-only resources rather than dropping them from the table. Report competing unmanaged installations when they affect a managed tool. `doctor <tool-or-resource>` expands one component, including an explicitly requested unmanaged tool without adopting it. Do not scan every user's files or enumerate unrelated repositories.

The report header identifies the machine/profile, platform/architecture, inspected working directory, effective PATH context, and observation time. Executable selection is for the invoking process and that directory, including supported version-manager shims. It is not proof of another shell's aliases, cached command lookup, environment, or a running application's selection. Distinguish a legitimate project-local version-manager override from a broken global installation or accidental PATH shadowing before flagging a version mismatch.

#### Required diagnostic fields

| Field group | Information |
| --- | --- |
| Identity and ownership | Tool/resource ID, owning module/source, package/update provider, and managed/trial/unmanaged disposition. Join observations by declared component/resource identity, not by guessing ownership from executable basenames. |
| Versions | Declared policy or resolved pin; installed versions; selected executable version; running version where safely observable. Preserve the distinction between installed, selected, and actually running. Retained old versions are not automatically ownership conflicts. |
| Locations | Executable selected from PATH, its shim/symlink chain and resolved backing binary; declared config destination, file/link/generated/merged kind, actual resolved target, and authoritative source path. A config-only resource has no executable, not a missing executable. |
| Observed state | Existence, executable permissions, relevant architecture/compatibility, config syntax/drift, link topology, appropriate permission checks, service/reload state when supported, and last-applied/source revision. Each probe records its time and freshness. |
| Findings | Text severity, stable diagnostic code, concise evidence/cause, and a suggested next action. Show missing dependencies, PATH shadowing, conflicting providers, broken links, stale generated files, invalid settings, and pending native updates distinctly. |

The default terminal table shows **Tool / owner**, **Status**, **Versions**, **Executable**, and **Config**. Put actionable findings immediately below the relevant row or in a keyed findings list. Wrap long paths or use labelled multiline rows on narrow terminals; do not hide the decisive path through irreversible truncation. Detailed selection and versioned `--json` output retain all fields and provenance without ANSI formatting.

Illustrative rows only, with invented tools, versions, paths, and codes. This is not a scan of the current machine. The shipped codes use the `D_` prefix ([docs/doctor.md](docs/doctor.md)).

| Tool / owner | Status | Versions | Executable | Config |
| --- | --- | --- | --- | --- |
| `sample-cli` / `personal-dev`, Homebrew | WARN: PATH_SHADOWED | Wanted 2.1; installed 2.1; selected 1.9 | Selected `/usr/local/bin/sample-cli`; intended `/opt/homebrew/bin/sample-cli` | `~/.config/sample-cli/config.toml` |
| `sample-config` / `core`, filesystem | ERROR: BROKEN_LINK | Not applicable | Not applicable | `~/.config/sample/config.toml` points to a missing source |

The first finding identifies the two executable paths and recommends reviewing PATH precedence, not reinstalling a tool that is already installed correctly. The second identifies the broken link and recommends checking its declared source before generating a repair plan.

#### Read-only and failure behavior

1. Perform bounded local metadata checks and only adapter-approved, side-effect-free probes with timeouts. Never run an arbitrary discovered binary or unresolved shim merely to obtain `--version`: it may initialize state, run plugins, auto-install, or make network requests. Unsupported or unsafe probes are explicitly skipped/unknown, not evidence of health.
2. Do not fetch, update, install, render secrets, authenticate to vaults, repair links, rewrite config, or restart services. Suggested commands are printed, never executed. Determine remote update availability only from dated cached metadata, otherwise report unknown. Matching an installed pin is not evidence that it is the latest upstream release.
3. Use explicit `OK`, `WARN`, `ERROR`, and `UNKNOWN` health results, plus `not applicable` for absent concepts. An unverified required check cannot produce `OK`. Unknown optional information remains visible without inventing a failure or a successful runtime check. A queued update is pending, not complete.
4. A missing module, malformed manifest, unavailable provider, failed probe, or probe timeout produces a diagnostic finding and does not suppress independent checks. Label any last-known receipt-derived inventory as stale/partial instead of presenting it as the current desired configuration. If resolution is broken, report the resolution error and whatever can be inspected safely; never silently report a healthy core-only subset.
5. Print paths and safe metadata, not config contents, credentials, secret values/hashes, environment dumps, or unredacted provider stderr. Reports stay local unless the user exports them; paths and module names may still be private even after secret redaction. JSON output preserves explicit unknowns, evidence freshness, and partial-report state. Use exit 0 when required checks complete without errors (warnings remain visible), 1 for diagnosed errors or incomplete required checks, and 2 for invalid invocation or inability to produce a diagnostic report. Table and JSON use the same result semantics.

Acceptance must prove useful diagnosis in broken environments, not just a pretty success table. Repair remains a separately reviewed `plan`/`apply`, `sync`, `update`, or other explicit action.

### Local state and editable sources

Local state lives under `~/.local/state/dotfiles-manager/`, outside configuration repositories. It records bindings, source identities, ownership claims, operation receipts, active trial records, and applicable snapshots with restrictive permissions. State records are versioned. State loss must not turn unknown files into owned files; recovery is inspect/adopt or receipt restoration, not forced application or guessed purge.

Raw edits through a supported link remain ordinary Git changes. A replaced link is a conflict with recoverable content, not permission to force-relink. Adoption can promote content to the source, make an exposed override, or leave the path unmanaged after review. Unsupported reverse transformations are reported explicitly.

## 8. Packages, secrets, removal, and recovery

- Choose one package/service/update owner per component. Observing Homebrew, Nix, or another installation is not authorization to switch ownership. Native update coordination remains with applications that require it.
- Config application does not independently refresh rolling versions. Package apply can install missing prerequisites or apply a changed declared version; `update` refreshes eligible versions and their owned pins/locks across sources. Apply/sync honor the resulting exact pins; explicit holds and soak policies remain enforced. A package binary rollback is not a rollback of an application's migrated database.
- Secret plans show approved references, destination names, and permission checks only. Vault authentication and secret rendering occur during explicit application. No secret bytes in plans, logs, Git, or generic capture; no public hashes of secret material. Temporary files and any permitted recovery material remain private.
- Removing a module removes desired ownership, not arbitrary files. Delete only exact previously owned outputs whose current identity/content still matches the receipt, and only when removal appears in the approved plan. Unknown or modified outputs are conflicts. Do not recursively delete directories, uninstall packages, or erase runtime state by inference.
- Snapshots and restoration are scoped to operations whose adapters support them. Restoring an ordinary file can be supported; external updates, asynchronous native actions, or data migrations may not be reversible. Report recovery instructions and partial progress rather than claiming universal rollback.

## 9. Examples and acceptance fixtures

`examples/base` is public and contains no reference to `examples/external`. The synthetic external bundle imports the base and adds `work`. Each file in `examples/profiles` describes a different machine.

| Profile | Selected role | Resolver expectation |
| --- | --- | --- |
| `personal-dev-01` | Personal development | Core, agents, desktop, personal-dev; package/native-update/secret declarations remain data. |
| `work-01` | Synthetic external module | Core, agents, desktop, work; permitted Git editor and complete-file overrides, optional skill disabling, handler and native-fragment contributions. |
| `homelab-01` | Homelab | Core, server, homelab; server dependency does not install the personal development stack. |
| `server-01` | Server | Core and server; explicit alternate editor default, no agent or desktop modules. |

The homelab and server profiles are illustrative schema fixtures (section 1, items 7 and 8).

Fixture values illustrate mechanisms, not final decisions about which real settings belong in core. The source files are intentionally small. The examples do not reproduce the complete current dotfiles inventory, provider dependencies, or private wrapper.

`examples/cases.toml` contains expected resolved module lists, selected effective values, and negative scenarios. Mutations use RFC 6902 operations on parsed fixture documents in memory. They must never be applied to the checked-in examples or live files. The first error follows validation order: document/bundle schema, selection/dependency validity, composition authority/conflicts, resolved adapter schema/source containment, then output claims/observations.

`scripts/check_examples.py` validates fixture syntax and references, not these resolver outcomes. `cargo test -p dfm-cli --test resolver` executes all resolver cases against in-memory copies. Passing those alone does not satisfy the required first-implementation workflows.

Additional apply acceptance requirements are already determined by existing failures: an isolated HOME, stale/dangling parent rejection, directory-to-file migration, replaced-link preservation, foreign-hook preservation, malformed-input last-good preservation, failed/empty secret injection, state-loss refusal to delete, source/target changes after planning, no network from read-only scopes, and explicit queued-versus-completed native updates.

### First-implementation workflow acceptance

These are required end-to-end tests, not outcomes established by the fixture checker or the resolver cases. Run them with isolated HOME directories, temporary Git remotes, and fake provider/vault/native-update processes before any approved live migration.

| Requirement | Required evidence |
| --- | --- |
| Complete cross-module update | One `update` accounts for core and nested external-module Git revisions, package/input pins, runtimes, plugins/extensions, skills, and native applications. Eligible pins advance in their owning sources; holds and soak exclusions are explicit. No provider is silently omitted. |
| Update preparation and interruption | A missing private credential or unsupported provider blocks deployment and pin publication during preparation. Declined approval changes no deployed files, owning source content, or installed versions; staged metadata is not deployment. A provider failure or pending native update cannot produce a full-success result; retry does not duplicate completed side effects. |
| One-command pull and apply | New core and external-module revisions are fetched, validated, and applied by one `sync`, including generated outputs and required version changes. Local edits and unrelated files survive. A repeated sync is a no-op where supported. |
| Safe Git/source publication | Divergence, invalid fetched manifests, lost credentials, newly untrusted sources, and changed observations block publication. Test with already-linked source files to prove validation occurs before checkout changes affect HOME. Never fall back silently to stale sources. |
| Trial then adoption | Try a previously unknown-to-the-profile tool without authoring a module, tune its configuration, adopt it into a chosen module, and reproduce it with sync. Preserve the baseline on adoption failure and exclude secrets/runtime state from source. |
| Trial then purge | Confirm exact cleanup, remove a trial-only installation and approved trial-created files, and restore a safe baseline. Preserve a pre-existing installation, shared dependencies, foreign state, and concurrent edits. Missing receipts or unsafe parents cannot authorize deletion. |
| Persistent trial ownership | A trial survives restart and ordinary sync/update without becoming a second profile or losing its baseline. Unsupported or newly discovered side effects remain visible; purge reports retained effects rather than claiming complete cleanup. |
| Diagnostic table and provenance | One `doctor` includes core/external-module tools, active trials, missing tools, and config-only resources. Verify wanted/installed/selected versions, PATH shadowing, shim/link targets, config/source paths, and module/provider ownership in table and JSON output. |
| Diagnostic failure and non-mutation | Malformed manifests, missing modules, timeouts, unsafe shims, conflicting installations, broken links, stale output, and unavailable runtime evidence yield explicit findings and appropriate nonzero results for errors/incomplete required checks. Independent rows remain visible. Assert no writes, network/vault access, installation, reload, or unsafe probe execution, and no secret leakage in either output format. |

## 10. Existing behavior that constrains migration

Source baseline: public dotfiles revision `1dbd12d3a563120a77aa3e27f68ad903a6d47d9e`. Current source takes precedence over historical memory.

Status (2026-10-20, `docs/roadmap.json`): the live M5 cutover ran on 2026-10-16, and Nix is no longer on the M5 (DFM-27 superseded). For the M5, the Nix rows and the Nix API rule below are a record. For the M3, DFM-40 and DFM-41 own the handoff.

| Existing implementation | Requirement retained or intentionally separated |
| --- | --- |
| `modules/home/raw-dotfiles.nix`, `scripts/check-home-link-parents.sh` | Reject stale parent links before writes. Preserve source files during ownership handoff. |
| `modules/home/ai-stack.nix` | Managed settings and handler retirement coexist with foreign state and seed-only preferences. |
| `modules/home/dotfiles.nix` | Explicit whole-file substitution, native seams, and ordered Codex/Pi instruction assembly. |
| `mcp_sync/src/mcp_sync/{sync,drift,capture,skills}.py` | Distinguish wholesale and co-owned outputs, preserve bounded capture semantics, validate source trust, and fail closed on required overlays. |
| `home/.local/bin/op-render` | Explicit interactive rendering, secure output, last-good preservation, and complete-render status. |
| `scripts/sync-side-channels.sh`, `scripts/update-inputs.sh` | Preserve separate native owners and declared update intent; a queued application update is not completion. |
| `docs/external-overlays.md` | Preserve dependency direction and current external API/seams until a reviewed replacement contract is deployed. |
| `hosts/personal-mac.nix` | Device-specific journal data belongs outside universal core; modularization can remove the need for some old overrides. |

The new manager must be able to replace link selection, activation-embedded generators, capability resolution, configuration diagnostics, and orchestration as their owners are handed off. It should not merely run a second implementation alongside them. Existing merge/distribution algorithms can be reused without retaining their old orchestration as another authority.

Keep the public Nix API working during migration. Release each old writer before enabling a new writer for the same output. Packages, services, privileged system paths, and the Nix store require their own ownership handoff. Successful fixture checks do not establish deployed compatibility, fresh-login behavior, or the accepted native menu/SketchyBar interactions.

The current private wrapper has not been inspected in this workspace. Synthetic external examples exercise the known public contract; they are not a compatibility certification. Its current declarative inputs and live ownership must be checked before migration.

## 11. Decisions and implementation status

### Decided

- Names (owner, 2026-10-20, DFM-67): the command is `dfm`. The project, crates, `dotfiles-manager.lock`, and the state, config, cache and data directories keep the name `dotfiles-manager`.
- The owner decisions of 2026-10-09 settle the implementation language (Rust), the build-from-source distribution, the direct crates, and the exit codes.

### Still open

These items stay open for owner review:

- TOML schema and the exact catalogue/override syntax.
- Which real configurations are universal core, exposed defaults, or optional modules.
- Production adapter packaging.
- Source-lock representation, native-provider coverage, and compatibility details for the current private wrapper.

These are review questions, not implicit permission to modify deployed configuration. They do not change the four workflows in section 1, which are fixed first-implementation requirements.

### Built

The section 7 table maps each workflow to its shipped command. Built: `bind`, `binding`, `state`, `explain`, `plan`, `doctor` and `status` (files and state only), `apply`, `secrets`, `packages` (with `--update` for rolling mise and uv tools), `services`, `trial`, `sync` (configuration scope), `self-update` and `self-check`. Not built: `update`, `adopt` and `purge`. [README.md](README.md) links the notes for each command.
