# Sync

The `sync` command: pull the latest configuration and bring the machine to it: configuration, packages, secrets and services, under one approval. This document describes the behavior at HEAD. Apply is in [docs/apply.md](apply.md), the source loader in [docs/resolver.md](resolver.md).

| Part | Location |
| --- | --- |
| The lock `[[sources]]` entries, their validation, the canonical origin check and the origin grants document | `crates/model/src/lock.rs` |
| `state/grants.json` read and write | `crates/engine/src/store.rs` (`read_grants`, `write_grants`) |
| The command: lock, trust, git through the runner, staging, rehearsal, the four scope plans, publication, then each scope | `crates/cli/src/sync_cmd.rs` |
| One scope's run, its probes and its pre-lock checks (`run`, `probe`, `check_before_lock`) | `crates/cli/src/apply_cmd.rs` |
| A plan built from a staged copy that names the live root | `crates/cli/src/lib.rs` (`Loaded`, field `staged`), `crates/cli/src/plan_cmd.rs` |
| Tests with temporary local remotes and `brew`, `op` and `launchctl` look-alikes | `crates/cli/tests/sync.rs` |

Git is the `git` binary on the runner's fixed search path (docs/secrets.md, "How `op` is run"). A Homebrew git in `/opt/homebrew/bin` wins over `/usr/bin/git`. Git never runs through a shell or with the caller's environment.

## The command

```
dfm sync [--root <dir>] [--preview] [-y|--yes-plan <digest>] [--allow-disruptive <operation-id>]...
         [--allow-adopt <operation-id>]... [--restart <resource-id>]... [--trust-origin <origin>]...
```

- `--root` is the user's checkout: the git working tree that holds the bound profile and `dotfiles-manager.lock`. It defaults to the root the binding records (docs/bootstrap.md).
- The flags of `apply` keep their meaning (docs/apply.md). Each `--allow-disruptive` and `--allow-adopt` id goes to the scope whose plan has that operation. An id that names no planned operation of that kind in any scope is a usage error. The digest that `sync` prints and `--yes-plan` takes is the **sync digest** (step 3), not a scope plan's.
- `--restart <resource-id>` goes to the services scope, as for `services` (docs/lifecycle.md). The restart is disruptive and needs its `--allow-disruptive` flag. An id that names no service is a usage error.
- `--trust-origin <origin>` grants one canonical origin. It is a usage error with `--preview` (a preview writes nothing) or with `--yes-plan` (one command never both widens trust and publishes).

## The four scopes

After publication, `sync` runs four scopes in this order. Each is its own command's run, with the same plan, gates, lock, journal and receipt:

1. **config**: `apply` (docs/apply.md).
2. **packages**: `packages` without `--update`: install what is missing (docs/packages.md).
3. **secrets**: `secrets` (docs/secrets.md).
4. **services**: `services`: start what is not loaded, and restart on `--restart` (docs/lifecycle.md).

Each scope plans from the published tree exactly as its standalone command does, and writes its own receipt (`command` = `apply`, `packages`, `secrets` or `services`) when it changed something. A scope whose plan changes nothing does not run. A scope that fails stops the scopes after it: the summary reports them `not_started`, and the run exits with the failure (exit 1 when an earlier scope or a publication changed something).

Two kinds of scope do not run and stop nothing:

- **Secrets without a session.** In the review, the secrets scope runs the session check of `secrets` (`op whoami` per account, `secret::missing_sessions`). When `op whoami` exits nonzero for an account, the planned secret resources are listed `skipped: no session` in the preview and in the summary, and the other scopes run. Before the secrets scope runs, the check runs again: a session that ended since the review gives the same result. Only that answer skips. An `op whoami` that times out or is ended by a signal is `E_CREDENTIAL_UNAVAILABLE`, and an `op` that is missing or fails its executable checks is `E_PROBE_FAILED`: either refuses the review, or fails the scope. `dfm secrets` on its own still refuses a locked vault (docs/secrets.md).
- **A blocked packages, secrets or services plan.** The scope is listed `blocked` with each blocked operation and its code, in the preview and in the summary, and the other scopes run. A blocked configuration plan still refuses the whole run before publication.

In both cases the last lines of standard output, after the `result:` line, are one `warning:` line per such scope. It names the skipped resources, each account with what `op whoami` answered, or the blocked operations, and the fix: unlock 1Password, then run `dfm sync` or `dfm secrets` again, or the blocker's own fix text and the standalone command. A skip found only when the secrets scope starts names `dfm secrets` alone, because those secrets were re-renders and a plain `dfm sync` does not render them again. The exit is 0 when every scope that ran succeeded. A preview ends with the same lines after the digest. A run that refuses (a missing approval, a stale digest, a failed scope) ends with its refusal and prints no warning.

The services scope is planned against the tree as it will be after publication and the configuration scope:

- A launchd definition that resolves into `--root` is read from the staged copy, so the preview, its `content:` precondition and "the definition runs ..." show what publication puts there. When the configuration plan of the same run changes the definition's link to another target, the definition is read at that new target.
- A service whose definition or configuration file is not published yet, while the configuration plan of the same run publishes it, is not blocked. The preview shows it as a start "after the configuration scope publishes" the file. When `--restart` names it, the preview shows it as a restart, and its `--allow-disruptive` flag is checked before publication: without the flag the run is refused before anything is published.
- A launchd agent that is loaded from the file its definition link resolved to before the configuration plan retargets that link is loaded from an earlier definition inside `--root`. It is current, not foreign (docs/lifecycle.md), in the review, after the configuration scope, and in every later run. A requested restart loads the new definition.

After an earlier scope of the same run ended `ok` with its approved digest, `sync` plans each later scope (packages, secrets, services) again from the published files. That plan must be the reviewed one with the effects of the earlier scopes' approved operations applied:

- A directory that an earlier scope created drops out of the reviewed plan. Example: the configuration and the secrets plans both create a missing `.config/example`.
- A `path:` or `sensitive-path:` precondition on a path that an earlier scope wrote must be the state that observe records for what that operation leaves there, formatted as the planner formats it (`node_state`): a directory it created (mode 700), the absence it removed to, the link it published (its target and what the target is), or the file it rendered (the declared mode, or 600). A non-sensitive file must hold the approved content (`sync` reads it again and compares its SHA-256 with the plan's), and its state is the hash or the size form. A `content:` precondition on a rendered file must be the approved SHA-256. A `content:` precondition through a link stays as reviewed, because the review read the new target.
- The deferred services are now starts (or current, or restarts when they were reviewed as restarts).
- Every other operation and precondition, the sources and the model are equal, and nothing blocks.

Then the scope runs with that plan's digest, and its first look uses that plan instead of probing again. The plan under the scope's lock is still made and compared with the digest. Otherwise it fails with `E_PLAN_STALE`, and the next `sync` plans it. When no earlier scope ran, a scope runs with its reviewed digest, so the comparison covers every precondition and observation.

A secrets plan never says `current` for an existing destination: the content exists only after the render (docs/secrets.md). A secrets plan whose planned operations only render again destinations the ledger proves (no new file, no takeover) is therefore treated as no change: it needs no approval on its own, the review runs no `op whoami` for it, and the summary says `current, not rendered again`. An approved run renders them, and a result equal to the file writes nothing. So a `dfm sync` with nothing else to change does not contact the vault, and a rotated secret is rendered by the next approved sync or by `dfm secrets`. Nothing is skipped on that path, so it prints no warning.

## What the lock declares

`dotfiles-manager.lock` at `--root` carries `[[sources]]` entries:

```toml
[[sources]]
id = "dotfiles"
kind = "git"
origin = "https://github.com/example/dotfiles"   # canonical form, see below
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
- A **nested source** (any other `path`) is an **exact** pin. `commit` is required, must be reachable from `ref` in the fetched history, and is what gets checked out. Sync never changes the pin or writes the lock. `update` (DFM-31, not built) is to advance pins.
- Paths are compared after the APFS case fold (`dfm_model::fold`). Two paths that differ only by case are a duplicate, and a path under a case alias of another source lies inside it. Both are `E_MANIFEST_INVALID`. Unicode normalization forms are not folded (docs/planner.md, known limits).
- **The fetched root's lock decides the nested sources.** Sync reads the live lock for the root entry only, fetches the root, then reads the lock at the fetched revision (for a checkout ahead of its ref, at its HEAD) for the nested entries. The fetched lock must name the same origin and ref for the root, else `E_SOURCE_UNTRUSTED`: a source may not redirect itself.
- A nested checkout is a git checkout with its own `.git` at `<root>/<path>`. Every component of `path` that already exists, under the root and under the staged copy, must be a real directory. A symlink is `E_SOURCE_ESCAPE`, anything else is `E_CONFLICT`. The loader refuses a `.git` below a source root (docs/resolver.md, item 8), so the bundle that the profile names must exclude it: `[sources] exclude = ["base/work/.git"]`.
- A lock that carries `tree` is refused (`E_MANIFEST_INVALID`), because this build does not compare it.
- A lock without `[[sources]]` is `E_MANIFEST_INVALID`, exit 3. Only `kind = "git"` is supported.

## Order of one run

1. **Binding, lock, trust.** The binding names the profile, and `--root` must contain it. The live lock is read. Every `--trust-origin` value must be canonical (S2: `https` or `ssh`, lowercase host, no userinfo except `git` for `ssh`, no trailing `/`, no `.git`, no query or fragment), else `E_SOURCE_UNTRUSTED`. A new grant is written under the writer lock, before any fetch, between a `grant-origin` journal `intent` entry and its `done` entry. An origin already granted writes nothing. The root origin must have a grant, else exit 3 `E_SOURCE_UNTRUSTED` with the `--trust-origin` command to run, and nothing is fetched.
2. **Copy and rehearse.** The review pass takes the shared manager lock (`E_LOCKED` when a writer holds it) and keeps it while its staging directory exists.
   - The checkout at `--root` is copied as it is (dirty, untracked and ignored files, nested checkouts, `.git`) into `<cache dir>/staging/<pid>-<start>`. The directory is a real directory owned by the user, mode 0700, and is removed when the run ends. The run holds a lock file inside it.
   - Only the run that holds the writer lock (step 5) removes sibling `<pid>-<start>` directories whose lock nobody holds. A review pass removes only its own.
   - In the copy: `git fetch` of the root's ref from its origin, then `git merge --ff-only` to the fetched head.
   - The fetched lock is read from the copy. Every nested origin is checked against the grants (exit 3 before any nested fetch when one is missing). Each nested source is fetched and fast-forwarded to its pin, or checked out fresh when the checkout does not exist yet.
   - **A checkout ahead of its ref is current.** When a checkout's HEAD already holds the fetched head or the pin under local commits (a branch with unpushed work), sync does not move it. Sync never moves a checkout back. Its digest line has old = new = HEAD, publication leaves it where it is, and the configuration is applied from the checkout as it is.
   - What git refuses in the copy, it would refuse in the user's checkout: divergence (`Not possible to fast-forward`), a local change to a file the update changes, an untracked file the update would create. The refusal is `E_CONFLICT`, exit 3, with git's own lines. The user's checkout is not touched.
3. **Plans and the sync digest.** The staged copy is resolved and planned as the tree `--root` will hold after publication, once per scope: config, packages, secrets, services. The packages and services plans probe their providers read-only first, as their commands do. Then the secrets session check runs (see "The four scopes"). The sources are read from the copy, and link targets and each plan's `source:` line name `--root`. The **sync digest** is the SHA-256 of one line per scope plan (`plan <scope> <plan digest>`), one line per skipped secret resource (`skip secrets <resource>`) and one line per source (`id origin ref old new`). A change in any plan changes it. It covers the commits that publication moves between, not only the tree they produce: a commit that changes only files outside the scoped roots leaves the plan digests unchanged. The preview prints each scope under a `scope:` heading with its own operations (the `not_in_scope` ones belong to another scope), then the one `plan digest:` line. Then, per scope that will run, the machine check, the plan blockers, the gates that need no write, the scope's `--allow-disruptive` and `--allow-adopt` ids and the journal capacity are checked with the code `apply` uses before its lock (`apply_cmd::check_before_lock`). The missing approvals of every scope are printed as one list of flags to paste (removals, restarts, adoptions), then the run exits 3. `--preview` ends here with exit 0 and the sync digest.
4. **Approval.** Without `--yes-plan`: the plans are printed, exit 3 `E_APPROVAL_REQUIRED`, nothing published. Exception: when every source is `current` and no scope that would run changes anything (docs/apply.md, step 4), the run prints `nothing to change: no approval needed`, the summary and `result: ok`, and exits 0. A digest that is not the sync digest is `E_PLAN_STALE`, exit 3.
5. **Publication, under the writer lock.** A second copy, a second rehearsal and a second plan of each scope. The sync digest must still match, else `E_PLAN_STALE`, exit 3, nothing published. This rechecks the sources immediately before publication, and catches an upstream that moved between preview and approval, also a new commit over the same tree. Then, for each source in order, root first:
   - The checkout's HEAD must still be what the copy saw (`E_PLAN_STALE`).
   - The new commit is fetched from the staged copy into the checkout, and the checkout is fast-forwarded (`git merge --ff-only`). A nested checkout that does not exist is created with `git init` and `git checkout --detach`.
   - HEAD is read back as the postcondition. A checkout that is already at its commit prints `current` and is left alone.
6. **Scopes.** The lock is released, and each scope runs in process, in order, with its own allow flags and `--yes-plan <plan digest>`, the scope plan digest the sync digest was built from. It plans `--root` (now the published tree), compares to the approved digest, takes the lock, plans again and runs (docs/apply.md). A scope after an earlier scope that ran is planned again first (see "The four scopes"). A stop request between two scopes starts no later scope: they are `not_started`, and the run exits 130 or 143.

Output: one `source:` line per source, the scope plans and the digest, `published:` lines, then for each scope that runs a `scope:` line and its report (the operations of other scopes left out) without a `result:` line of its own. Then `scopes:` with one line per scope (`ok`, `current`, `skipped: no session (...)`, `blocked (...)`, `not_started`, or the exit and code of a failure), the `result:` line and the `warning:` lines. The change in a `source:` line is one of:

- `<old>..<new>`
- `current at <commit>`
- `current at <head> (ahead of <commit>)` for a checkout ahead of its ref or pin
- `new checkout at <commit>`

followed by `(tracking)` or `(exact)`.

## Exit codes

| Code | When |
| --- | --- |
| 0 | Published (or nothing to publish) and every scope that ran succeeded, or nothing to change. Skipped and blocked scopes (other than config) do not change the exit. A repeat sync with nothing new publishes nothing and writes no receipt. |
| 1 | After some publication or an earlier scope: a later source failed to publish (`sync: published a; b was not published; no scope was run`), or a scope refused or failed. Also each scope's own exit 1 cases. |
| 2 | Usage, `--trust-origin` with `--preview` or with `--yes-plan`, a non-UTF-8 path. |
| 3 | Before any publication: untrusted origin, a fetch that failed (`E_SOURCE_MISSING`), a pin not in its ref (`E_SOURCE_MISSING`), a conflict in the rehearsal (`E_CONFLICT`), an invalid lock or composition, a blocked configuration plan, a gate refusal of a scope that would run, a provider probe that failed, a stale digest, a missing approval, the lock held. |
| 5 | The services scope ended `pending` (docs/lifecycle.md), and nothing failed. |
| 130, 143 | A signal during a scope (docs/apply.md). The fetch, probe and publication phases install no handler (see Known limits). |

## Git isolation

### Fetch environment

Built from empty:

- `PATH` (the search path), `HOME` = `<cache dir>/sync-home` (empty, manager-owned), `LANG=C`.
- `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_SYSTEM=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`, `GIT_TERMINAL_PROMPT=0`, `GIT_NO_REPLACE_OBJECTS=1`.
- `GIT_ALLOW_PROTOCOL=https:ssh` for a fetch from an origin, and `file` for the local fetches between the copy and the checkout. A `test-isolation` build allows `file` origins, so tests use bare repositories in a temporary directory.

Every call carries `-c http.followRedirects=false -c core.hooksPath=/dev/null -c core.fsmonitor=false -c transfer.fsckObjects=true -c core.useReplaceRefs=false -c core.autocrlf=false -c core.eol=lf`.

`http.followRedirects=false` makes an `https` origin that answers with a redirect fail its fetch, because the redirect target is a host the grant never named. The same setting closes the dumb-HTTP alternates path. A server without smart HTTP makes `git-remote-https` fall back to the dumb walker, which reads `objects/info/http-alternates` and `objects/info/alternates`, and an entry there may name another host. Git's `is_alternate_allowed` (`http-walker.c`) admits an alternate only when `http.followRedirects` is `true`. A fetch that needs one fails with `E_SOURCE_MISSING`. A unit test asserts the setting and that no other `http.followRedirects` value is set.

No credential helper is configured. An `https` origin that needs one fails its fetch (`E_SOURCE_MISSING`, exit 3), because prompts are off.

### Self-contained checkouts

Every checkout the command drives (the staged copy and the live one, root and nested) must keep its git directory inside itself (`rev-parse --absolute-git-dir`) and have itself as top level (`rev-parse --show-toplevel`), else `E_SOURCE_UNTRUSTED`. A linked worktree (a gitfile), a separate git directory or `core.worktree` would otherwise send the rehearsal or the publication to another place.

### Checkout configuration

The scrubbed environment removes the global and system configuration, but every git command in a checkout still reads its `.git/config` (with `.git/config.worktree` and whatever `include.path` names). A key there can send a fetch to another host (`url.*.insteadOf`, `http.proxy`, `remote.*.proxy`), run a command (`credential.helper`, `core.sshCommand`, `core.gitProxy`, `core.askPass`, `alias.*`, `core.fsmonitor`) or rewrite the files a fast-forward writes (`filter.*`, `core.attributesFile`, `core.symlinks`). That would bypass the exact-origin grant.

So before any git command runs in a checkout, `git config --list --show-origin` is read through the runner. Every key outside an allowlist is `E_SOURCE_UNTRUSTED`, named by key with its subsection elided, because a value or a `url.<base>` subsection can hold a credential. The `-c` settings show origin `command line:` and are skipped.

The allowlist is what `git init` and `git clone` write, plus the identity keys:

- `core.repositoryformatversion`, `core.filemode`, `core.bare`, `core.logallrefupdates`, `core.ignorecase`, `core.precomposeunicode`
- `remote.<name>.url`, `remote.<name>.fetch`, `remote.<name>.pushurl`
- `branch.<name>.remote`, `branch.<name>.merge`, `branch.<name>.vscode-merge-base`
- `user.name`, `user.email`
- the whole `worktrunk.*` and `coderabbit.*` sections (tool namespaces git never reads)

A `remote.<name>.*` key is allowed only under a plain nickname (no `/`, no `:`). Git matches a fetch argument against the configured nicknames before it treats it as a URL, so `[remote "https://host/x"] url = elsewhere` would redirect a fetch of `https://host/x` (tested). A canonical origin always holds `://` and a staging path starts with `/`, so neither can equal a plain nickname. The listed `branch.*` keys are inert, because `merge --ff-only <commit>` and `checkout --detach <commit>` read no remote. `branch.<name>.mergeOptions` and `.rebase` stay refused, because `mergeOptions` reaches `merge`. `user.*` is inert, because sync never commits.

To keep an unlisted key, move it out of the checkout: `git config --unset <key>` in the checkout, then `git config --global <key> <value>`. Sync never reads the global file; the user's own git still does.

A non-empty `.git/info/attributes`, `.git/info/grafts` or `.git/shallow` is also `E_SOURCE_UNTRUSTED` (`bootstrap.sh install` has the same rule for attributes). Grafts and a shallow file rewrite the ancestry that `merge-base --is-ancestor` and `merge --ff-only` report, so a graft could move HEAD over local-only history. `GIT_NO_REPLACE_OBJECTS` does not cover them.

## Decisions

| Question | Choice |
| --- | --- |
| Where sources are declared | The `[[sources]]` of `dotfiles-manager.lock` at `--root`, plus `path` (where a nested checkout lives, default `.`). The binding's `origins` field stays unsupported. |
| Tracking versus exact | By position: the root source tracks (no `commit`), every nested source is exact (`commit` required). |
| Which lock names the nested sources | The fetched root revision's lock. The live lock's nested entries are ignored. The root entry must match between the two. |
| Trust without an interactive prompt | No prompt exists. `--trust-origin <origin>` is the explicit grant: it names the origin, is refused with `--preview`, and `--yes-plan` never grants. A grant may be written for an origin that no lock names yet. It has no effect until a lock names it. |
| Grant record | `state/grants.json` (state schema 2): `{origin, grant_id, granted_at}`, replaced atomically under the writer lock between a `grant-origin` journal `intent` (op id = the grant id, resource = the origin, no recovery outputs) and its `done` entry. The grants are read again under the lock before an origin is tested or added, so two concurrent `--trust-origin` runs cannot overwrite each other's grant (tested with the `sync-before-grant-lock` failpoint). A run that dies in between leaves the intent open, which `doctor`, `status` and `state recover` report. |
| Staging is a copy of the checkout | The plan digest covers the loaded source tree, which holds the user's dirty and untracked files. A staging tree built from the fetched commit alone would never match the tree after publication. The copy keeps them, so the rehearsal and the plan see exactly what publication produces, and git's own fast-forward rules decide conflicts the same way in both places. A socket or FIFO inside `.git` (a file monitor) is skipped; elsewhere it is `E_SOURCE_UNTRUSTED`. At most 1,000,000 entries. |
| Approval bound to commits | The sync digest covers each source's `(id, origin, ref, old, new)`, so a `--yes-plan` from a preview is stale after any new upstream commit. The second rehearsal under the lock recomputes it and also compares the per-source commits with the review pass. |
| A checkout ahead of its ref | Current, not moved back (DFM-70). The digest line has old = new = HEAD. |
| Two locks, one approval | The writer lock is held for the rehearsal and the publication, released, then taken again by each scope. Another manager command can run between them. Each scope's digest comparison refuses a changed plan, and sync reports it in the summary and exits 1. |
| One run for four scopes (DFM-63) | DESIGN.md section 7 asks for one combined preview and approval. Each scope keeps its own command path, plan, receipt and failure rules, so no second planner or executor exists. The order (config, packages, secrets, services) follows the prerequisites: a service needs its published definition and its installed formula. |
| A locked vault | Skipped in `sync`, refused in `secrets`. The daily run should not stop for a locked 1Password app. The warning at the end of standard output names what waits (owner decision 2026-10-26). |
| A blocked later scope | It does not run, the others do, and the warning names it (owner decision 2026-10-26). One exited daemon or one unknown package probe then does not hold back the configuration. A blocked configuration plan still refuses the run, because publication without the configuration would leave the checkout ahead of HOME. |
| Re-renders on a run with nothing else new | Not approved and not rendered. The plan cannot tell a re-render that writes nothing from one that writes. A rotated vault value reaches the file at the next approved sync or `dfm secrets`. |
| Partial publication | Sources publish in lock order, root first. A failure after one source published is reported by name and exits 1. The checkouts already moved stay moved: a fast-forward is not undone. |
| Failed fetch, missing pin | `E_SOURCE_MISSING`, exit 3, before any plan. No fallback to the checkout's current content, and no digest is printed. A pin absent from its ref is refused with the head named and never substituted. |
| Read-only commands | `plan`, `doctor`, `status`, `explain` and `state check` read `--root` as it is and never fetch. A test keeps it so. |

## Known limits

- **Signals.** The gate test allows `signal::install` only in `apply_cmd.rs` and `trial_cmd.rs`, so sync's fetch, rehearsal, probe and publication phases run with no handler. The first scope that runs installs them. A SIGINT ends the manager and its git child together, because the child shares the manager's process group before the handlers exist. A stop during a fast-forward leaves what git left. Git updates refs last, so the checkout is at the old or the new commit, and the next sync rehearses again. The staging directory of a stopped run stays until the next approved run removes it. The kernel releases its lock.
- **Tree digest.** The lock's `tree` is not recomputed or compared (refused when present). Commit identity rests on git's object hashing with `transfer.fsckObjects=true`.
- **Copy cost.** Each approved run copies the whole checkout twice (review, then under the lock). A checkout with large ignored trees (`node_modules`, virtual environments) is slow to copy.
- **Credentials.** No credential helper from the binding (the binding's `origins` and helper fields are not implemented). `ssh` origins use the user's `~/.ssh` through the password database. The host-key policy is a device-gate item.
- **A later scope can go stale.** A scope plans from the published tree after the earlier scopes ran. A change that the check of the earlier scopes' effects above does not cover makes its plan differ from the approved one, and it fails with `E_PLAN_STALE` (exit 1). Example: a launchd definition that the configuration scope generates (not a link into `--root`) and whose program changes: the preview read the old file, so "the definition runs ..." differs. Run `dfm sync` again.
- **A brew service whose formula the same run installs.** It is blocked (`Missing`) in the review and starts at the next sync. Only a definition or configuration file that the configuration scope publishes defers a service.
- **Probe cost.** A run probes the packages and services providers twice in the review and under the lock, and each scope that runs probes twice more (the recheck after an earlier scope replaces the scope's first look).
- **Receipts.** Each scope writes its own receipt, and `sources` lists the path-kind identity only. Sync prints the git commits that publication moved between, but state does not record them. The grant is in the journal and `grants.json`, not in a receipt (`origin_grants[]` waits for the sync receipt of DFM-31, a `shortcut` in `sync_cmd.rs`). `doctor` has no row for an unreceipted grant.
- **Nested checkout branches.** A new nested checkout is created detached at its pin. An existing one is fast-forwarded on whatever HEAD it has, so a branch moves with it.
- **Evidence scope.** Tests use the `test-isolation` build, a temporary HOME, `file://` bare remotes and `/usr/bin/git`. No test runs an `https` or `ssh` fetch or a GitHub origin. The DFM-70 commit message records a `dfm sync` run against the live `~/.dotfiles` on the personal M5 on 2026-10-24.
