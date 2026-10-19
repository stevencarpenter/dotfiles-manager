# Lifecycle (DFM-26)

Task: DFM-26, the `launchd-agent/v1` and `brew-service/v1` service adapters and the `services` command. This document records what the first service lifecycle does and where it resolves or differs from [docs/architecture.md](architecture.md) (the contract) and DESIGN.md sections 5 and 8. The contract wins where this document is silent. The planner is in [docs/planner.md](planner.md), apply in [docs/apply.md](apply.md), packages in [docs/packages.md](packages.md).

A published file or an accepted launcher request is not proof that the program uses it. `services` asks each service's own launcher what it holds, acts through that launcher only, asks again, and records what it observed.

## What exists

| Part | Location |
| --- | --- |
| `Scope::Services`, `ServiceState`, `service_prerequisites`, `Action::Start`, `Action::Restart`, the service decision (`Planner::service`) and its edges (`service_edges`) | `crates/model/src/plan/` |
| Probes (`launchctl print`, `brew services info --json`), the definition label check, the actions (`launchctl bootstrap`, `bootout`, `brew services start`, `restart`) and the bounded completion wait | `crates/cli/src/adapters/service.rs` |
| `Generator::reloads`, `Generator::service_state`, `Generator::service_act`, `Completion` | `crates/engine/src/generate.rs` |
| Execution (`try_service`: prerequisite re-check, re-probe, intent, action, postcondition, outcome) and the `pending` outcome | `crates/engine/src/apply.rs` |
| `services` command (the `apply` command over the services scope), `--restart` | `crates/cli/src/apply_cmd.rs`, `crates/cli/src/lib.rs` |
| Code `E_PENDING` (exit 5); receipt reload states `reloaded` and `requested` | `crates/model/src/refusal.rs`, `crates/model/src/state.rs` |
| Tests with `launchctl` and `brew` look-alikes | `crates/cli/tests/services.rs` |

The child-process runner is the one `secrets` and `packages` use (docs/secrets.md, "How `op` is run"): fixed search path, checked executable, rebuilt environment, own process group, timeout, SIGTERM then SIGKILL on a stop request. Firstmate (`firstmate-native/v1`) is not inventoried on the personal M5 or the work M3 and has no adapter; its catalog entry stays declaration-only.

## The command

```
dotfiles-manager services --root <dir> [--preview] [--yes-plan <digest>] [--allow-disruptive <operation-id>]... [--restart <resource-id>]...
```

`services` is `apply` over the services scope: every `service` resource is planned, every other resource is `not_in_scope` ("link is configuration, outside a services plan"), and nothing is removed or adopted (`--allow-adopt` is a usage error). Planning needs each launcher's report, so **`services` probes the launchers before it plans**, read-only, and hands the answers to the plan builder (nothing in `plan_cmd.rs` starts a process; the gate test keeps it so). The signal handlers go in before the first probe. `plan`, `apply`, `packages`, `secrets` and `doctor` start no launcher and list a service as `not_in_scope`.

Order of one run: resolve, observe the prerequisite files, probe, plan and print; the gates that need no write, with every restart listed as a pasteable `--allow-disruptive '<id>'`; without `--yes-plan` exit 3; the lock, the second plan (the probes run again); one operation per service; receipt (`command = services`) and report.

`--restart <resource-id>` plans a restart of a service that is current. It is for a configuration the program reads only at start (the status bar after its config changed): the manager cannot see what a running process loaded, so it does not guess. The request is part of the plan (the operation's action and effect, so the digest covers it), and the restart needs `--allow-disruptive` like any other. An id that names no service in the plan is a usage error.

## The adapters

| Adapter | Value | Probe (read-only) | Current when | Start | Restart |
| --- | --- | --- | --- | --- | --- |
| `launchd-agent/v1` | `label` (letters, digits, `.`, `_`, `-`, not starting with `-`, at most 128 characters), `plist` (the HOME-relative path of the definition, a published file or link) | the definition is resolved and read (1 MiB bound) to check that its `Label` string is the declared label; then `launchctl print gui/<uid>/<label>`: exit 0 gives the loaded `path` and `state`, exit 113 ("Could not find service") means not loaded; 60 s | launchd holds the label and its `path`, with links resolved, is the declared definition. The run state (`running`, `not running`) is reported, not required: a one-shot agent (`RunAtLoad`, exits) is not running after it ran | `launchctl bootstrap gui/<uid> <HOME>/<plist>` | `launchctl bootout gui/<uid>/<label>` (a label the domain does not hold is accepted), a wait of at most 10 s until `launchctl print` no longer finds it (launchd tears a service down asynchronously), then the bootstrap |
| `brew-service/v1` | `name` (the formula: letters, digits, `.`, `_`, `-`, `+`, `@`), `config` (optional, HOME-relative files the program reads) | `brew services info <name> --json` with `HOMEBREW_NO_AUTO_UPDATE=1`, `HOMEBREW_NO_ANALYTICS=1`, `HOMEBREW_NO_ENV_HINTS=1`: `loaded`, `running`, `status`; exit 1 with "No available formula" or "is not installed" means the formula is not installed; 60 s | `loaded` and `running`: a Homebrew service is a daemon | `brew services start <name>`, then at most 10 s of `info` reports until `running` | `brew services restart <name>`, the same wait |

Actions have 120 s. A probe that cannot run, times out, or answers in a shape the adapter does not know is `Unknown` with a reason: the plan has a blocker, `E_PROBE_FAILED`, exit 3 (architecture section 8 item 9: unknown support fails closed). Launcher standard error is bounded and never printed or persisted (the message names its size and the command to run by hand).

**Never run:** `launchctl unload`, `launchctl remove`, `launchctl kickstart`, `brew services stop`, `brew services kill`, `brew services remove`, `brew services cleanup`, `brew uninstall`, `brew services run`, anything under `sudo` or in the `system` domain. A bootout happens only inside a restart, with its bootstrap following in the same operation.

## State model

Four facts are kept apart, as DESIGN.md section 5 asks ("a correct file, an installed executable and a reloaded consumer are separate results"):

| State | launchd agent | Homebrew service | Where it shows |
| --- | --- | --- | --- |
| installed | the definition file is published at `plist` (observed by the planner, no process) and declares the label | brew knows the formula (`brew services info` answers) | a missing one is a blocker with fix text; the service is ordered after the operation that publishes or installs it (`after:` in the plan) |
| selected | the resource is in the bound profile | the same | a deselected service gets no operation: nothing is stopped or unloaded (see Decisions) |
| loaded | `launchctl print` finds the label; its `path` is the declared definition | `loaded = true` | the probe state: `current:…`, `stopped:…`, `stale:…` in the precondition and the effect text |
| running | `state = running` in the report (`pid` present) | `running = true` | reported for an agent, required for a Homebrew service |

The probe answers map to one of:

| `ServiceState` | Meaning | Plan |
| --- | --- | --- |
| `Current(detail)` | loaded as declared (and running, for brew) | `current`, no action; with `--restart`: a disruptive `restart` |
| `Stopped(detail)` | not loaded | `start`, planned, not disruptive (nothing of it is running) |
| `Stale(detail)` | loaded from another definition (launchd), or loaded and not running (brew) | `restart`, planned, disruptive |
| `Missing(why)` | the definition is not published, or the formula is not installed | blocker `E_CONFLICT`: run `apply` or `packages` first |
| `Unknown(why)` | the launcher did not answer, or not in a known shape | blocker `E_PROBE_FAILED` |
| `Refused(why)` | the declaration is wrong: bad label or name, a definition that declares another label, a definition that is not an XML property list | blocker `E_MANIFEST_INVALID` |

The detail strings carry no process id and no time, because the probe state is the precondition apply compares after it probes again under the lock.

## One operation

1. **Plan time.** For each path in `plist` and `config`, the planner observes the HOME path (hash on). A missing path, or a link that dangles or loops, blocks the service (`E_CONFLICT`, "depends on …, which is not published; a reload depends on the published file"). Every present one becomes a `path:<file>` precondition with its observed state. The probe becomes `probe:<adapter>:<name> = <state>`. The operation id is `<resource>@service`; it has no path, no mode and no ledger entry. `after` lists the operations of the resources whose claims are those files and, for a Homebrew service, the package resource of the same name: `<resource>@<path>`, `<resource>@package`, or their `@scope` entries when those resources are outside this plan.
2. **Apply.** The prerequisite files are observed again and compared (`E_PLAN_STALE` blocks when one changed), then the launcher is asked again: the planned state still holds, or a `start` whose service is now current is `current`, or `Unknown` blocks with `E_PROBE_FAILED`, or anything else blocks with `E_PLAN_STALE`. Then the stop check, the `intent` entry (no recovery output), the action with its bounded wait, and the launcher's report as the postcondition.
3. **Outcome.** The report says current: `done`, status `updated`, `reload = reloaded`. The launcher accepted the request (brew: loaded) and did not report running within the bound: `pending`, status `pending`, `reload = requested`, `E_PENDING`, the run goes on and exits 5 when nothing else fails. Any other report, a nonzero exit, a timeout: `failed`, exit 1. A stop request that cuts off the postcondition is reported as the stop, and the journal says the request may have completed.
4. **Receipt.** Class `normal`, no fingerprints, no owned outputs, the reload state above, an `approvals[]` entry with `how = allow_disruptive` for a restart that ran.

A `pending` entry closes the intent (architecture section 5: `pending` is a terminal phase). The next `services` run probes again: a brew service that is loaded and not running is `Stale`, so a restart is planned and needs approval; one that came up meanwhile is `current`.

## Exit codes

| Exit | When |
| --- | --- |
| 0 | every service is `current` or `updated` |
| 1 | a start or restart `failed` (launcher error, timeout, postcondition not met) |
| 2 | usage: `--allow-adopt`, `--restart` on another command, a `--restart` or `--allow-disruptive` id that names nothing in the plan |
| 3 | blocked before any write: a prerequisite not published or not installed, an unknown launcher state, a wrong declaration, a stale plan, a restart without `--allow-disruptive` (`E_DISRUPTIVE_UNAPPROVED`), no `--yes-plan` (`E_APPROVAL_REQUIRED`) |
| 5 | `E_PENDING`: a request the launcher accepted without observed completion, and no failure |
| 130, 143 | a stop request; a probe in flight is ended, no further launcher starts |

## Decisions where the documents were silent

| Question | Choice |
| --- | --- |
| Command shape | A separate `services` command, as `packages` and `secrets` are separate. DESIGN.md names `sync` as the command that composes "necessary reload scopes"; `sync` is DFM-30, and `services` is the lifecycle part of it. The `after` edges already name the publishing operations, so a combined plan orders and blocks correctly. |
| What "current" means for launchd | The domain holds the label from the declared definition file (paths compared with links resolved, since launchd reports the resolved path). launchd keeps the definition it loaded; whether that cached definition equals the file now is not observable, so an edited definition is reloaded on request (`--restart`), never by guess. |
| Reloading after a configuration change | Not inferred. The manager cannot observe what a running process read. `--restart <resource>` is the explicit, approved request; the owner's `tiling-reload.sh` restarted the bar unconditionally after every apply, which this replaces with a reviewed step. A combined plan (DFM-30) can turn a planned prerequisite operation into a restart; this leaf does not, so nothing restarts without a request or a stale state. |
| `kickstart` | Not used. It restarts the process under the definition launchd already holds, which is not a reload of an edited definition. A restart is bootout, wait, bootstrap. |
| Disruptive set | A restart (bootout, `brew services restart`) is disruptive and needs `--allow-disruptive <operation-id>`, as the contract lists (section 10 item 8). A start of something that is not loaded is not disruptive: nothing is running. The flag's refusal text now says "removal or restart". |
| Pending | `E_PENDING` (exit 5) was added: the contract names exit 5 and status `pending` but no code. The run continues past a pending operation (independent operations are not dependents of it); no operation in this scope depends on a service, so `blocked_pending` is never produced here. |
| Reload states | `reloaded` and `requested` were added to the fixed list in `state.rs`; docs/apply.md's statement that only `not_attempted` is accepted predates this. A published file still records `not_attempted`; the report prints its "reload: not attempted" line only for those. The receipt validator accepts a reload state on an `allow_disruptive` operation (a restart) beside the `before` fingerprint of a removal. |
| Deselected service | Nothing. A service that is no longer selected gets no operation: nothing is stopped, unloaded or removed (DESIGN.md section 8: no removal by inference; a deselected agent keeps running until the owner unloads it by hand). |
| Competing launchers | The adapter checks that the published definition declares the declared label, so a bootstrap never loads another agent under this resource; a label loaded from another file is reported as stale and replaced only with approval. A second agent with another label for the same purpose (the old `org.nixos.maxfiles`) is not detected: nothing here lists the domain. |
| Application data and sessions | A restart replaces the process and nothing else: no file of the program is read, written or removed, no `brew uninstall`, no `launchctl remove`. The two inventoried services (the maxfiles agent, the status bar) hold no data. |
| Version or data compatibility | Not applicable to these two adapters: a start or restart has no irreversible step (the definition on disk is the one that is loaded, and the previous process state is the launcher's). No version is compared because none is declared; an adapter for a program with a migrating data store would need a check here and should refuse without one. |
| Brew without a running process | A Homebrew service that is loaded and not running (`status error`, exited) is stale and gets a restart, not a start: `brew services start` on a loaded service reports it already started. |
| Prerequisite that changed between plan and apply | The locked re-plan sees the change first (digest mismatch, `E_PLAN_STALE`); the engine checks the `path:` preconditions again immediately before the action as well (section 8: re-verify before each operation). |
| Interrupted action | As for a package: the intent has no path, so recovery cannot probe it, and the resource blocks (`E_OP_INTERRUPTED`) until `state resolve <journal-id> --as failed`, even when the launcher reports it loaded. |

## Known limits

- **Not observable: what a running process loaded.** `current` for launchd means "the domain holds the declared file"; for brew it means "loaded and a process runs". Neither proves the process read the current file content. Use `--restart` after a configuration change that the program reads only at start.
- **Rerun cost.** Every `services` run probes every service twice (plan and locked re-plan), and a start or restart probes again afterwards, up to the completion bound.
- **Bounded waits.** A brew daemon that needs more than 10 s to start is `pending`; a launchd teardown longer than 10 s fails the restart before its bootstrap (nothing is loaded, the operation is `failed`, and the agent is unloaded until the next run starts it). The isolated build reads `DFM_SERVICE_TIMEOUT_SECS` for the tests.
- **Domain.** Only `gui/<uid>` (the current user's launch agents). No daemon, no `system` domain, no other user.
- **Brew's text.** "Not installed" is read from brew's standard error ("No available formula", "is not installed"); another wording is `Unknown`, which blocks.
- **A changed source behind a link.** The `plist` precondition of a linked definition is the link target, not the content: editing the file in the checkout does not make a saved plan stale, and the owner decides when to `--restart`.
- **Evidence scope.** Tests use the `test-isolation` build, a temporary HOME and `launchctl` and `brew` look-alikes shaped from the real tools' output (`launchctl print` of `org.carpenter.maxfiles` and `brew services info sketchybar --json` on the personal M5, read-only, 2026-10-18). No real agent was loaded and no real service was started by a test. Native evidence before cutover is an owner step: `services --preview` on the M5 against `~/.dotfiles` with the maxfiles agent and the sketchybar service declared, then one approved run, then `launchctl print gui/$(id -u)/org.carpenter.maxfiles` and `brew services list` by hand.
