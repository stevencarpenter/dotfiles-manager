# Sync (DFM-30)

Task: DFM-30, the `sync` command. This document records what the first pull-and-apply does and where it resolves or differs from [docs/architecture.md](architecture.md) (the contract, sections 5, 8 and 10) and DESIGN.md ("Pull the latest configuration and apply it"). The contract wins where this document is silent. The planner is in [docs/planner.md](planner.md), apply in [docs/apply.md](apply.md), the resolver and source loader in [docs/resolver.md](resolver.md).

## What exists

| Part | Location |
| --- | --- |
| The lock `[[sources]]` entries, their validation, the canonical origin check and the origin grants document | `crates/model/src/lock.rs` |
| `state/grants.json` read and write | `crates/engine/src/store.rs` (`read_grants`, `write_grants`) |
| The command: lock, trust, git through the runner, staging, rehearsal, plan, publication, then `apply` | `crates/cli/src/sync_cmd.rs` |
| A plan built from a staged copy that names the live root | `crates/cli/src/lib.rs` (`Loaded::staged`), `crates/cli/src/plan_cmd.rs` (one read-root line) |
| Tests with temporary local remotes | `crates/cli/tests/sync.rs` |

No dependency was added. Git is the git binary found on the runner's fixed search path, run through the bounded child-process runner of `secrets` and `packages` (docs/secrets.md, "How `op` is run"), never through a shell and never with the caller's environment.

## The command

```
dotfiles-manager sync --root <dir> [--preview] [--yes-plan <digest>] [--allow-disruptive <operation-id>]...
                      [--allow-adopt <operation-id>]... [--trust-origin <origin>]...
```

`--root` is the user's checkout: the git working tree that holds the bound profile and `dotfiles-manager.lock`. The flags of `apply` keep their meaning (docs/apply.md), except that the digest `sync` prints and `--yes-plan` takes is the **sync digest** (below), not the configuration plan's. `--trust-origin` grants one canonical origin and cannot be combined with `--preview` (a preview writes nothing) or with `--yes-plan` (one command never both widens trust and publishes); both are usage errors.

### What the lock declares

`dotfiles-manager.lock` at `--root` carries `[[sources]]` entries in the contract's shape (section 5, "Source lock"), with two readings this task fixes:

```toml
[[sources]]
id = "dotfiles"
kind = "git"
origin = "https://github.com/example/dotfiles"   # canonical form, section 8
ref = "refs/heads/main"                           # the tracking ref; no commit: the root tracks
                                                  # path defaults to "." (the root checkout)

[[sources]]
id = "work"
kind = "git"
origin = "ssh://git@github.com/example/work-modules"
ref = "refs/heads/main"
commit = "<40 hex>"                               # exact: must be reachable from ref
path = "base/work"                                # a nested checkout inside the root
```

- The **root source** (`path = "."`, exactly one) is the **tracking** source. It holds the lock, so it carries no `commit`: sync advances it to the head of `ref`. A root entry with a `commit` is `E_MANIFEST_INVALID`.
- A **nested source** (any other `path`) is an **exact** pin: `commit` is required, must be reachable from `ref` in the fetched history, and is what gets checked out. Sync never changes it; `update` (DFM-31) is the command that advances pins.
- The **fetched root's lock decides the nested sources.** Sync reads the live lock for the root entry only, fetches the root, then reads the lock at the fetched revision for the nested entries ("the module revisions required by the fetched configuration", DESIGN.md). The fetched lock must name the same origin and ref for the root, else `E_SOURCE_UNTRUSTED` (a source may not redirect itself).
- A nested checkout is a git checkout with its own `.git` at `<root>/<path>`. Every component of `path` that already exists, under the root and under the staged copy, must be a real directory: a symlink is `E_SOURCE_ESCAPE` and anything else is `E_CONFLICT`, before any checkout is created. The loader refuses a `.git` below a source root (docs/resolver.md, item 8), so the bundle that the profile names must exclude it: `[sources] exclude = ["base/work/.git"]`.
- `tree` is parsed and **not compared** by this build; a lock that carries one is refused (`E_MANIFEST_INVALID`) rather than half-checked (known limit below).
- A lock without `[[sources]]` has nothing to sync: `E_MANIFEST_INVALID`, exit 3. A `kind = "path"` source is not supported.

### Order of one run

1. **Binding, lock, trust.** The binding names the profile; `--root` must contain it. The live lock is read. Every `--trust-origin` value must be canonical (section 8, S2: `https` or `ssh`, lowercase host, no userinfo except `git` for `ssh`, no trailing `/`, no `.git`, no query or fragment), else `E_SOURCE_UNTRUSTED`. A new grant is written under the writer lock, before any fetch, between a `grant-origin` journal `intent` entry and its `done` entry (an origin already granted writes nothing). The root origin must have a grant, else exit 3 `E_SOURCE_UNTRUSTED` with the `--trust-origin` command to run, and nothing is fetched.
2. **Copy and rehearse.** The review pass takes the shared manager lock (`E_LOCKED` when a writer holds it) and keeps it while its staging directory exists. The checkout at `--root` is copied, as it is (dirty files, untracked files, ignored files, nested checkouts, `.git`), into a per-run staging directory `<cache dir>/staging/<pid>-<start>` (a real directory owned by the user, mode 0700, removed when the run ends). The run holds a lock file inside it; before creating its own, every run removes a sibling `<pid>-<start>` directory whose lock nobody holds (a run that died), and skips anything else. In the copy: `git fetch` of the root's ref from its origin, then `git merge --ff-only` to the fetched head. Then the fetched lock is read from the copy, every nested origin is checked against the grants (exit 3 before any nested fetch when one is missing), and each nested source is fetched and fast-forwarded to its pin in the same way, or checked out fresh when the checkout does not exist yet. What git refuses in the copy, it would refuse in the user's checkout: divergence (`Not possible to fast-forward`), a local change to a file the update changes, an untracked file the update would create. The refusal is `E_CONFLICT`, exit 3, with git's own lines, and the user's checkout was not touched.
3. **Plan and the sync digest.** The staged copy is resolved and planned as the tree `--root` will hold after publication: the sources are read from the copy, link targets and the plan's `source:` line name `--root`. The **sync digest** is the SHA-256 of the configuration plan's digest plus one line per source (`id origin ref old new`), so it covers the commits that publication moves between, not only the tree they produce (the plan digest covers the loaded source tree, and a commit that changes only files outside the scoped roots leaves it unchanged). The machine check, the plan blockers, the gates that need no write, `--allow-disruptive` and `--allow-adopt` and the journal capacity are checked with the same code `apply` uses before its lock (`apply_cmd::check_before_lock`), before anything is published. `--preview` ends here with exit 0 and the sync digest.
4. **Approval.** Without `--yes-plan`: the plan is printed, exit 3 `E_APPROVAL_REQUIRED`, nothing published. With a digest that is not the sync digest: `E_PLAN_STALE`, exit 3.
5. **Publication, under the writer lock.** A second copy, a second rehearsal and a second plan; the sync digest must still match (`E_PLAN_STALE`, exit 3, nothing published: this is the recheck of source and target identities immediately before publication, and it catches an upstream that moved between preview and approval, also a new commit over the same tree). Then, for each source in order, root first: the checkout's HEAD must still be what the copy saw (`E_PLAN_STALE`), the new commit is fetched from the staged copy into the checkout, and the checkout is fast-forwarded (`git merge --ff-only`) or, for a nested checkout that does not exist, created with `git init` and `git checkout --detach`. HEAD is read back as the postcondition. A checkout that is already at its commit prints `current` and is left alone.
6. **Apply.** The lock is released and `apply --root <dir> --yes-plan <plan digest>` runs with the same allow flags, where the plan digest is the configuration plan's digest the sync digest was built from: it plans `--root` (now the published tree), compares to the approved digest, takes the lock, plans again and applies (docs/apply.md). The receipt has `command = apply`.

Output: one `source:` line per source (`<id> <origin> <ref> <old>..<new> (tracking|exact)`, or `current at <commit>`, or `new checkout at <commit>`), the plan when the run does not go on to apply, `published:` lines, then apply's own plan, report and `result:` line.

### Exit codes

| Code | When |
| --- | --- |
| 0 | Published (or nothing to publish) and applied, including no work needed. A repeat sync with nothing new publishes nothing, applies nothing and writes no receipt. |
| 1 | After some publication: a later source failed to publish (`sync: published a; b was not published`), or apply refused or failed after the sources were published (`sync: published ...; the configuration was not applied`). Apply's own exit 1 cases. |
| 2 | Usage, `--trust-origin` with `--preview` or with `--yes-plan`, a non-UTF-8 path. |
| 3 | Before any publication: untrusted origin, a fetch that failed (`E_SOURCE_MISSING`), a pin not in its ref (`E_SOURCE_MISSING`), a conflict in the rehearsal (`E_CONFLICT`), an invalid lock or composition, a blocked plan, a stale digest, a missing approval, the lock held. |
| 130, 143 | A signal during apply (docs/apply.md). During the fetch and publication phases no handler is installed (below). |

## Decisions where the documents were silent or this task deviates

| Question | Choice |
| --- | --- |
| Where sources are declared | The `[[sources]]` of `dotfiles-manager.lock` at `--root`, in the contract's lock shape, plus `path` (where a nested checkout lives, default `.`). The binding's `origins` field stays unsupported. |
| Tracking versus exact | By position: the root source tracks (no `commit`), every nested source is exact (`commit` required). C06: the tracking ref advances to its head; the pin is checked out exactly; the lock is never written by sync. |
| Which lock names the nested sources | The fetched root revision's lock. The live lock's nested entries are ignored. The root entry must match between the two. |
| Trust without an interactive prompt | No prompt exists in this build (as DFM-21 recorded). `--trust-origin <origin>` is the explicit grant: it names the origin, is refused with `--preview`, and `--yes-plan` never grants (the contract's rule). A grant may be written for an origin that no lock names yet; it is harmless until a lock names it. |
| Grant record | `state/grants.json` (state schema 2): `{origin, grant_id, granted_at}`, replaced atomically under the writer lock between a `grant-origin` journal `intent` (op id = the grant id, resource = the origin, no recovery outputs) and its `done` entry. A run that dies in between leaves the intent open, which `doctor`, `status` and `state recover` report as an open entry. The receipt's `origin_grants[]` field waits for the sync receipt of DFM-31 (`shortcut` in `sync_cmd.rs`); `doctor` has no `GRANT_UNRECEIPTED` row of its own. |
| Git binary | Resolved like `op`, `brew` and `mise`: a plain name on the runner's fixed search path with the executable checks of section 8. A Homebrew git in `/opt/homebrew/bin` wins over `/usr/bin/git` when present. The isolated build takes the search path from `DFM_PROVIDER_PATH`. |
| Fetch environment | Built from empty: `PATH` (the search path), `HOME` = `<cache dir>/sync-home` (empty, manager-owned), `LANG=C`, `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_SYSTEM=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`, `GIT_TERMINAL_PROMPT=0`, `GIT_NO_REPLACE_OBJECTS=1`, `GIT_ALLOW_PROTOCOL=https:ssh` for a fetch from an origin and `file` for the local fetches between the copy and the checkout. Every call carries `-c core.hooksPath=/dev/null -c core.fsmonitor=false -c transfer.fsckObjects=true -c core.useReplaceRefs=false -c core.autocrlf=false -c core.eol=lf`. The isolated build allows `file` origins so tests use bare repositories in a temporary directory. No credential helper is configured; an `https` origin that needs one fails its fetch (`E_SOURCE_MISSING`, exit 3) because prompts are off. |
| Staging is a copy of the checkout | The plan digest covers the loaded source tree, which holds the user's dirty and untracked files. A staging tree built from the fetched commit alone would never match the tree after publication. The copy keeps them, so the rehearsal and the plan see exactly what publication produces, and git's own fast-forward rules decide conflicts once, the same way in both places. The copy includes `.git`; a socket or FIFO inside `.git` (a file monitor) is skipped, elsewhere it is `E_SOURCE_UNTRUSTED`. At most 1,000,000 entries. |
| Self-contained checkouts | Every checkout the command drives (the staged copy and the live one, root and nested) must keep its git directory inside itself (`rev-parse --absolute-git-dir`) and have itself as top level (`rev-parse --show-toplevel`), else `E_SOURCE_UNTRUSTED`. A linked worktree (a gitfile), a separate git directory or `core.worktree` would otherwise send the copy's rehearsal, or the publication, to another place. |
| Approval bound to commits | The sync digest covers each source's `(id, origin, ref, old, new)`, so a `--yes-plan` from a preview is stale after any new upstream commit, also one whose tree is unchanged or that touches only files outside the scoped roots. The second rehearsal under the lock recomputes it and, in process, also compares the per-source commits with the review pass. |
| Filters and attributes | As `bootstrap.sh install`: a checkout whose configuration sets `core.attributesFile` or any `filter.*`, or that has a non-empty `.git/info/attributes`, is `E_SOURCE_UNTRUSTED`, because a fast-forward writes files through them. |
| Packages and secrets | Not composed. `sync` is the `apply` (configuration) scope: a package or secret resource is `not_in_scope` in its plan, as in the contract's walkthrough for secrets. The owner runs `packages` and `secrets` after a sync when the plan lists them. C04's "fetched declared versions" of packages are therefore not applied by `sync` in this build. |
| Two locks, one approval | The writer lock is held for the rehearsal and the publication, released, then taken again by `apply`. Between the two, another manager command could run; `apply`'s digest comparison refuses a changed plan, and sync reports the published sources and exits 1. |
| Partial publication | Sources publish in lock order, root first. A failure after one source published is reported by name and exits 1; the checkouts already moved stay moved (a fast-forward is not undone). |
| Failed fetch, missing pin | `E_SOURCE_MISSING`, exit 3, before any plan: no fallback to the checkout's current content, no digest printed. A pin absent from its ref is refused with the head named and never substituted. |
| Read-only commands | Unchanged. `plan`, `doctor`, `status`, `explain` and `state check` read `--root` as it is and never fetch; a test keeps it so. |

## Known limits

- **Signals.** The gate test fixes `signal::install` to `apply_cmd.rs`, so sync's fetch, rehearsal and publication phases run with no handler: a SIGINT ends the manager and its git child together (the child shares the manager's process group before the handlers exist). A stop during a fast-forward leaves what git left; git updates refs last, so the checkout is either at the old or the new commit, and the next sync rehearses again. The staging directory of a stopped run stays until the next run removes it (its lock is released by the kernel). `apply` installs the handlers as before.
- **Tree digest.** The lock's `tree` is not recomputed or compared (refused when present). Commit identity rests on git's object hashing with `transfer.fsckObjects=true`.
- **Copy cost.** Each run copies the whole checkout twice (preview, then under the lock). A checkout with large ignored trees (`node_modules`, virtual environments) is slow to copy; exclude such trees from the sources and keep them out of the checkout, or accept the cost.
- **Credentials.** No credential helper from the binding (the binding's `origins` and helper fields are not implemented). `ssh` origins use the user's `~/.ssh` through the password database, as the contract records; the host-key policy is a device-gate item.
- **Receipt.** The receipt is apply's: `command = apply`, `sources` lists the path-kind identity only. The git commits that publication moved between are printed by sync and not recorded in state; the grant is in the journal and `grants.json` but not in a receipt. A sync receipt with `sources[]` git identities and `origin_grants[]` belongs with the lock-writing work of DFM-31.
- **Nested checkout branches.** A nested checkout is created detached at its pin. One that exists is fast-forwarded on whatever HEAD it has (a branch moves with it).
- **Evidence scope.** Tests use the `test-isolation` build, a temporary HOME, `file://` bare remotes and `/usr/bin/git`. No `https` or `ssh` fetch and no GitHub origin was run by a test.
