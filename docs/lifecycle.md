# Lifecycle

The `launchd-agent/v1` and `brew-service/v1` service adapters and the `services` command. This document describes the behavior at HEAD.

A published file or an accepted launcher request is not proof that the program uses it. `services` asks each service's own launcher what it holds, acts through that launcher only, asks again, and records what it observed.

| Part | Location |
| --- | --- |
| `Scope::Services`, `ServiceState`, `ServiceFacts`, `Definition`, `service_prerequisites`, `Action::Start`, `Action::Restart`, the service decision (`Planner::service`) and its edges (`service_edges`) | `crates/model/src/plan/` |
| Probes, the definition label check, the actions and the bounded completion wait | `crates/cli/src/adapters/service.rs` |
| `Generator::reloads`, `Generator::service_state`, `Generator::service_act`, `Completion` | `crates/engine/src/generate.rs` |
| Execution (`try_service`) and the `pending` outcome | `crates/engine/src/apply.rs` |
| `services` command (`apply` over the services scope) and `--restart` | `crates/cli/src/apply_cmd.rs`, `crates/cli/src/lib.rs` |
| Code `E_PENDING` (exit 5); receipt reload states `reloaded` and `requested` (`RELOADS`) | `crates/model/src/refusal.rs`, `crates/model/src/state.rs` |
| Tests with `launchctl` and `brew` look-alikes | `crates/cli/tests/services.rs` |

Launchers run through the bounded runner (docs/secrets.md, "How `op` is run"). `firstmate-native/v1` has a catalog entry and no adapter.

## The command

```
dfm services [--root <dir>] [--preview] [-y|--yes-plan <digest>] [--allow-disruptive <operation-id>]... [--restart <resource-id>]...
```

`services` is `apply` over the services scope. Every `service` resource is planned. Every other resource is `not_in_scope` ("link is configuration, outside a services plan"). Nothing is removed or adopted: `--allow-adopt` is a usage error. `--root` defaults to the root the binding records.

Planning needs each launcher's report, so `services` probes the launchers read-only before it plans. The plan builder gets the answers, and nothing in `plan_cmd.rs` starts a process. `plan`, `apply`, `packages`, `secrets` and `doctor` start no launcher.

Order of one run:

1. The signal handlers go in.
2. Resolve, observe the prerequisite files, probe, plan and print.
3. The gates that need no write. Every restart is listed as a pasteable `--allow-disruptive '<id>'`.
4. Without `--yes-plan`: exit 3. A plan where every service is `current` and the journal has no open entry needs no digest: it exits 0 (docs/apply.md, step 4).
5. The writer lock and the second plan. The probes run again.
6. One operation per service, then the receipt (`command = services`) and the report.

`--restart <resource-id>` plans a restart of a service that is current, or of a Homebrew service that is loaded and not running. It is the only way a restart is planned. Use it for a configuration the program reads only at start, or for a daemon that exited. The manager cannot see what a running process loaded, so it does not guess. The request is part of the plan (the operation's action and effect), so the digest covers it. The restart also needs `--allow-disruptive`. An id that names no service in the plan is a usage error.

## The adapters

| Adapter | Value | Probe (read-only, 60 s) | Current when | Start | Restart |
| --- | --- | --- | --- | --- | --- |
| `launchd-agent/v1` | `label` (letters, digits, `.`, `_`, `-`, not starting with `-`, at most 128 characters), `plist` (the HOME-relative path of the definition, a published file or link) | The definition is resolved and read (1 MiB bound) as an XML property list. The top-level `Label` string must be the declared label. Its SHA-256 becomes the `content:<plist>` precondition, and its `Program` or `ProgramArguments` goes into the effect text ("the definition runs …"). Then `launchctl print gui/<uid>/<label>`: exit 0 gives the top-level `path` and `state` (one-tab lines only), exit 113 ("Could not find service") means not loaded. | launchd holds the label, and its `path` (links resolved) is the declared definition. The run state is reported, not required: a one-shot agent (`RunAtLoad`, exits) is not running after it ran. A label loaded from any other file is foreign: a blocker, never a restart. | `launchctl bootstrap gui/<uid> <HOME>/<plist>` | Only on `--restart`: `launchctl bootout gui/<uid>/<label>` (a label the domain does not hold is accepted), a wait of at most 10 s until `launchctl print` no longer finds it, then the bootstrap. |
| `brew-service/v1` | `name` (the formula: letters, digits, `.`, `_`, `-`, `+`, `@`), `config` (optional, HOME-relative files the program reads) | `brew services info <name> --json` with `HOMEBREW_NO_AUTO_UPDATE=1`, `HOMEBREW_NO_ANALYTICS=1`, `HOMEBREW_NO_ENV_HINTS=1`: `loaded`, `running`, `status`. Exit 1 with "No available formula" or "is not installed" means the formula is not installed. | `loaded` and `running`. Loaded and not running is stale: a blocker until `--restart`. | `brew services start <name>`, then at most 10 s of `info` reports until `running` | Only on `--restart`: `brew services restart <name>`, the same wait. |

Actions have 120 s. A probe that cannot run, times out, or answers in a shape the adapter does not know is `Unknown`: the plan has a blocker, `E_PROBE_FAILED`, exit 3: unknown support fails closed. Launcher standard error is bounded and never printed or persisted. The message names its size and the command to run by hand.

**Never run:** `launchctl unload`, `launchctl remove`, `launchctl kickstart`, `brew services stop`, `brew services kill`, `brew services remove`, `brew services cleanup`, `brew uninstall`, `brew services run`, anything under `sudo` or in the `system` domain. A bootout happens only inside a restart, with its bootstrap in the same operation.

## State model

Four facts are kept apart (DESIGN.md section 5: "a correct file, an installed executable and a reloaded consumer are separate results"):

| State | launchd agent | Homebrew service | Where it shows |
| --- | --- | --- | --- |
| installed | The definition file is published at `plist` and declares the label | brew knows the formula (`brew services info` answers) | A missing one is a blocker with fix text. The service is ordered after the operation that publishes or installs it (`after:` in the plan). |
| selected | The resource is in the bound profile | The same | A deselected service gets no operation: nothing is stopped or unloaded. |
| loaded | `launchctl print` finds the label, and its `path` is the declared definition | `loaded = true` | The probe state: `current:…`, `stopped:…`, `stale:…` in the precondition and the effect text. |
| running | `state = running` in the report (`pid` present) | `running = true` | Reported for an agent, required for a Homebrew service. |

| `ServiceState` | Meaning | Plan |
| --- | --- | --- |
| `Current(detail)` | Loaded as declared (and running, for brew) | `current`, no action. With `--restart`: a disruptive `restart`. |
| `Stopped(detail)` | Not loaded | `start`, planned, not disruptive (nothing of it is running) |
| `Stale(detail)` | Loaded and not running (brew): a process the manager did not start exited | Blocker `E_CONFLICT` naming `--restart <resource>`. With `--restart`: a disruptive `restart`. |
| `Foreign(detail)` | Loaded from a definition that is not the declared one (launchd) | Blocker `E_CONFLICT` with the `launchctl bootout` command to run by hand. `--restart` does not apply. |
| `Missing(why)` | The definition is not published, or the formula is not installed | Blocker `E_CONFLICT`: run `apply` or `packages` first. |
| `Unknown(why)` | The launcher did not answer, or not in a known shape | Blocker `E_PROBE_FAILED` |
| `Refused(why)` | The declaration is wrong: bad label or name, a definition that declares another label, or a definition that is not an XML property list the reader understands (binary, malformed, a repeated key, an unknown element or entity) | Blocker `E_MANIFEST_INVALID`. The found label is never repeated, because the file can be a secret output. |

The detail strings carry no process id and no time, because apply compares the probe state again under the lock.

## One operation

1. **Plan time.** For each path in `plist` and `config`, the planner observes the HOME path (hashed, except for a sensitive path). A missing path, or a link that dangles or loops, blocks the service (`E_CONFLICT`, "depends on …, which is not published; a reload depends on the published file"). Every present path becomes a `path:<file>` precondition with its observed state (`sensitive-path:<file>`, mode only, for a sensitive one). A non-sensitive launchd definition also becomes `content:<plist> = sha256:<hash>` from the bytes the adapter read through the link, so an edit behind a stable link makes the plan stale. The probe becomes `probe:<adapter>:<name> = <state>`. The operation id is `<resource>@service`. It has no path, no mode and no ledger entry. `after` lists the operations of the resources whose claims are those files and, for a Homebrew service, the package resource of the same name: `<resource>@<path>`, `<resource>@package`, or their `@scope` entries when those resources are outside this plan.
2. **Apply.** The prerequisite files are observed again and compared, and the definition is read and hashed again. A change blocks with `E_PLAN_STALE`. Then the launcher is asked again. The planned state must still hold. A `start` whose service is now current is `current`. `Unknown` blocks with `E_PROBE_FAILED`. Anything else blocks with `E_PLAN_STALE`. Then the stop check, the `intent` entry (no recovery output), the action with its bounded wait, and the launcher's report as the postcondition.
3. **Outcome.**
   - The report says current: `done`, status `updated`, `reload = reloaded`.
   - The launcher accepted the request (brew: loaded) and did not report running within the bound: `pending`, status `pending`, `reload = requested`, `E_PENDING`. The run goes on and exits 5 when nothing else fails.
   - Any other report, a nonzero exit, a timeout: `failed`, exit 1.
   - A stop request that cuts off the postcondition is reported as the stop, and the journal says the request may have completed.
4. **Receipt.** Class `normal`, no fingerprints, no owned outputs, the reload state above, and an `approvals[]` entry with `how = allow_disruptive` for a restart that ran.

A `pending` entry closes the intent: `pending` is a terminal journal phase. The next `services` run probes again. A brew service that is still loaded and not running is `Stale`, a blocker until `--restart` is requested and approved. One that came up meanwhile is `current`.

An interrupted action follows the packages rule (docs/packages.md, "Interrupted install"): the resource blocks with `E_OP_INTERRUPTED` until `state resolve <journal-id> --as failed`, even when the launcher reports it loaded.

## Exit codes

| Exit | When |
| --- | --- |
| 0 | Every service is `current` or `updated`. |
| 1 | A start or restart `failed` (launcher error, timeout, postcondition not met). |
| 2 | Usage: `--allow-adopt`, `--restart` on another command, a `--restart` or `--allow-disruptive` id that names nothing in the plan. |
| 3 | Blocked before any write: a prerequisite not published or not installed, a label loaded from a foreign definition, a loaded daemon that is not running (without `--restart`), an unknown launcher state, a wrong declaration, a stale plan, a restart without `--allow-disruptive` (`E_DISRUPTIVE_UNAPPROVED`), no `--yes-plan` (`E_APPROVAL_REQUIRED`). |
| 5 | `E_PENDING`: a request the launcher accepted without observed completion, and no failure. |
| 130, 143 | A stop request. A probe in flight is ended, and no further launcher starts. |

## Decisions

| Question | Choice |
| --- | --- |
| Command shape | A separate `services` command, as `packages` and `secrets` are separate. `sync` does not compose it (docs/sync.md). The `after` edges already name the publishing operations, so a combined plan would order and block correctly. |
| What "current" means for launchd | The domain holds the label from the declared definition file. Paths are compared with links resolved, because launchd reports the resolved path. Whether launchd's cached definition equals the file now is not observable, so an edited definition is reloaded only on request (`--restart`). |
| A label loaded from another definition | A blocker, not a restart. The manager never unloads what it did not load. A hand-installed agent under the same label stays until the owner runs the `launchctl bootout` command the blocker names. An approval alone is not enough, because the approval text would not say whose definition goes away. |
| A loaded daemon that is not running | A blocker, not an automatic restart. The manager keeps no record of which process it started, so it cannot tell its own exited daemon from another owner's. Only `--restart` (disruptive, approved) restarts it. A service ledger of manager-started processes was not built: a process id is not a durable identity across reboots. |
| A sensitive prerequisite | A `plist` or `config` path that is a secret output or a sensitive ledger entry is recorded by mode only: `sensitive-path:<file> = file:<mode>`. Apply observes and compares it the same way, so no hash or size of the content reaches the plan, the journal or a message. For a secret definition, the plan also records no `content:` precondition and shows no "the definition runs" text, because a token among the arguments would otherwise be in the plan. A changed mode still makes the plan stale. A non-sensitive file over 16 MiB is recorded as `file:<mode>:size:<n>` and compared by size. |
| Definition content behind a link | The planner's `path:` precondition of a link covers the link target only. So the adapter reads the definition at plan time (the `content:` precondition and the effect text), and apply reads and hashes it again before the action. |
| Reloading after a configuration change | Not inferred. The manager cannot observe what a running process read. `--restart <resource>` is the explicit, approved request. Nothing restarts without a request or a stale state. |
| `kickstart` | Not used. It restarts the process under the definition launchd already holds, which does not reload an edited definition. A restart is bootout, wait, bootstrap. |
| Disruptive set | A restart (bootout, `brew services restart`) is disruptive and needs `--allow-disruptive <operation-id>`, as a removal does ([docs/apply.md](apply.md)). A start of something that is not loaded is not disruptive. |
| Pending | `E_PENDING` (exit 5) is the code for exit 5 and status `pending`. The run continues past a pending operation. No operation in this scope depends on a service, so `blocked_pending` is never produced here. |
| Reload states | `reloaded` and `requested` join `not_attempted` in `RELOADS`. A published file still records `not_attempted`. The receipt validator accepts `reloaded` on an updated `allow_disruptive` operation (a restart). |
| Deselected service | Nothing. Nothing is stopped, unloaded or removed (DESIGN.md section 8: no removal by inference). A deselected agent keeps running until the owner unloads it by hand. |
| Reading the definition's label | A strict XML plist reader in the adapter. The XML declaration, DOCTYPE and comments are skipped, a CDATA section is text, and only the top-level `<dict>` key and value pairs are read. A text search for `<key>Label</key>` would take a decoy in a comment, a CDATA section or a nested dictionary as the label, which launchd would not. Anything the reader does not understand is refused. A shape is named, never content. |
| Competing launchers | The adapter checks that the published definition declares the declared label, so a bootstrap never loads another agent under this resource. A second agent with another label for the same purpose is not detected: nothing here lists the domain. |
| Application data | A restart replaces the process and nothing else. No file of the program is read, written or removed. |
| Version or data compatibility | Not applicable to these two adapters: a start or restart has no irreversible step. An adapter for a program with a migrating data store needs a check here and should refuse without one. |
| Brew without a running process | A Homebrew service that is loaded and not running (`status error`, exited) is stale. With `--restart` it gets a restart, not a start, because `brew services start` on a loaded service reports it already started. |

## Known limits

- **What a running process loaded is not observable.** `current` for launchd means "the domain holds the declared file". For brew it means "loaded and a process runs". Neither proves the process read the current file content. Use `--restart` after a configuration change that the program reads only at start.
- **Rerun cost.** Every `services` run probes every service twice (plan and locked re-plan). A start or restart probes again afterwards, up to the completion bound.
- **Bounded waits.** A brew daemon that needs more than 10 s to start is `pending`. A launchd teardown longer than 10 s fails the restart before its bootstrap: the agent stays unloaded and the operation is `failed` until the next run starts it. A `test-isolation` build reads `DFM_SERVICE_TIMEOUT_SECS`.
- **Domain.** Only `gui/<uid>` (the current user's launch agents). No daemon, no `system` domain, no other user.
- **Brew's text.** "Not installed" is read from brew's standard error ("No available formula", "is not installed"). Another wording is `Unknown`, which blocks.
- **A `config` file behind a link.** A `config` file of a Homebrew service is covered by its `path:` precondition only (the link target for a link). Whether the running program read it is not observable. The owner decides when to `--restart`.
- **Evidence scope.** Tests use the `test-isolation` build, a temporary HOME, and `launchctl` and `brew` look-alikes shaped from the real tools' output (`launchctl print` of `org.carpenter.maxfiles` and `brew services info sketchybar --json` on the personal M5, read-only, 2026-10-18). No test loads a real agent or starts a real service. Native evidence is an owner step: `services --preview` on the M5 with the maxfiles agent and the sketchybar service declared, then one approved run, then `launchctl print gui/$(id -u)/org.carpenter.maxfiles` and `brew services list` by hand.
