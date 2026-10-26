# Sync

The `sync` command: pull the latest configuration and apply it. [docs/architecture.md](architecture.md) (sections 5, 8 and 10) is the contract and wins where this document is silent. Apply is in [docs/apply.md](apply.md), the source loader in [docs/resolver.md](resolver.md).

| Part | Location |
| --- | --- |
| The lock `[[sources]]` entries, their validation, the canonical origin check and the origin grants document | `crates/model/src/lock.rs` |
| `state/grants.json` read and write | `crates/engine/src/store.rs` (`read_grants`, `write_grants`) |
| The command: lock, trust, git through the runner, staging, rehearsal, plan, publication, then `apply` | `crates/cli/src/sync_cmd.rs` |
| A plan built from a staged copy that names the live root | `crates/cli/src/lib.rs` (`Loaded`, field `staged`), `crates/cli/src/plan_cmd.rs` |
| Tests with temporary local remotes | `crates/cli/tests/sync.rs` |

Git is the `git` binary on the runner's fixed search path (docs/secrets.md, "How `op` is run"). A Homebrew git in `/opt/homebrew/bin` wins over `/usr/bin/git`. Git never runs through a shell or with the caller's environment.

## The command

```
dfm sync [--root <dir>] [--preview] [-y|--yes-plan <digest>] [--allow-disruptive <operation-id>]...
         [--allow-adopt <operation-id>]... [--trust-origin <origin>]...
```

- `--root` is the user's checkout: the git working tree that holds the bound profile and `dotfiles-manager.lock`. It defaults to the root the binding records (docs/bootstrap.md).
- The flags of `apply` keep their meaning (docs/apply.md). The digest that `sync` prints and `--yes-plan` takes is the **sync digest** (step 3), not the configuration plan's.
- `--trust-origin <origin>` grants one canonical origin. It is a usage error with `--preview` (a preview writes nothing) or with `--yes-plan` (one command never both widens trust and publishes).

`sync` is the configuration scope only. A package, secret or service resource is `not_in_scope` in its plan. Run `packages`, `secrets` or `services` after a sync when the plan lists them.

## What the lock declares

`dotfiles-manager.lock` at `--root` carries `[[sources]]` entries in the contract's shape (section 5, "Source lock"):

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
- A **nested source** (any other `path`) is an **exact** pin. `commit` is required, must be reachable from `ref` in the fetched history, and is what gets checked out. Sync never changes the pin or writes the lock. `update` (DFM-31, not built) is to advance pins.
- Paths are compared after the APFS case fold (`dfm_model::fold`). Two paths that differ only by case are a duplicate, and a path under a case alias of another source lies inside it. Both are `E_MANIFEST_INVALID`. Unicode normalization forms are not folded (docs/planner.md, known limits).
- **The fetched root's lock decides the nested sources.** Sync reads the live lock for the root entry only, fetches the root, then reads the lock at the fetched revision (for a checkout ahead of its ref, at its HEAD) for the nested entries. The fetched lock must name the same origin and ref for the root, else `E_SOURCE_UNTRUSTED`: a source may not redirect itself.
- A nested checkout is a git checkout with its own `.git` at `<root>/<path>`. Every component of `path` that already exists, under the root and under the staged copy, must be a real directory. A symlink is `E_SOURCE_ESCAPE`, anything else is `E_CONFLICT`. The loader refuses a `.git` below a source root (docs/resolver.md, item 8), so the bundle that the profile names must exclude it: `[sources] exclude = ["base/work/.git"]`.
- A lock that carries `tree` is refused (`E_MANIFEST_INVALID`), because this build does not compare it.
- A lock without `[[sources]]` is `E_MANIFEST_INVALID`, exit 3. Only `kind = "git"` is supported.

## Order of one run

1. **Binding, lock, trust.** The binding names the profile, and `--root` must contain it. The live lock is read. Every `--trust-origin` value must be canonical (section 8, S2: `https` or `ssh`, lowercase host, no userinfo except `git` for `ssh`, no trailing `/`, no `.git`, no query or fragment), else `E_SOURCE_UNTRUSTED`. A new grant is written under the writer lock, before any fetch, between a `grant-origin` journal `intent` entry and its `done` entry. An origin already granted writes nothing. The root origin must have a grant, else exit 3 `E_SOURCE_UNTRUSTED` with the `--trust-origin` command to run, and nothing is fetched.
2. **Copy and rehearse.** The review pass takes the shared manager lock (`E_LOCKED` when a writer holds it) and keeps it while its staging directory exists.
   - The checkout at `--root` is copied as it is (dirty, untracked and ignored files, nested checkouts, `.git`) into `<cache dir>/staging/<pid>-<start>`. The directory is a real directory owned by the user, mode 0700, and is removed when the run ends. The run holds a lock file inside it.
   - Only the run that holds the writer lock (step 5) removes sibling `<pid>-<start>` directories whose lock nobody holds. A review pass removes only its own.
   - In the copy: `git fetch` of the root's ref from its origin, then `git merge --ff-only` to the fetched head.
   - The fetched lock is read from the copy. Every nested origin is checked against the grants (exit 3 before any nested fetch when one is missing). Each nested source is fetched and fast-forwarded to its pin, or checked out fresh when the checkout does not exist yet.
   - **A checkout ahead of its ref is current.** When a checkout's HEAD already holds the fetched head or the pin under local commits (a branch with unpushed work), sync does not move it. Sync never moves a checkout back. Its digest line has old = new = HEAD, publication leaves it where it is, and the configuration is applied from the checkout as it is.
   - What git refuses in the copy, it would refuse in the user's checkout: divergence (`Not possible to fast-forward`), a local change to a file the update changes, an untracked file the update would create. The refusal is `E_CONFLICT`, exit 3, with git's own lines. The user's checkout is not touched.
3. **Plan and the sync digest.** The staged copy is resolved and planned as the tree `--root` will hold after publication. The sources are read from the copy, and link targets and the plan's `source:` line name `--root`. The **sync digest** is the SHA-256 of the configuration plan's digest plus one line per source (`id origin ref old new`). It covers the commits that publication moves between, not only the tree they produce: a commit that changes only files outside the scoped roots leaves the plan digest unchanged. Then the machine check, the plan blockers, the gates that need no write, `--allow-disruptive`, `--allow-adopt` and the journal capacity are checked with the code `apply` uses before its lock (`apply_cmd::check_before_lock`). `--preview` ends here with exit 0 and the sync digest.
4. **Approval.** Without `--yes-plan`: the plan is printed, exit 3 `E_APPROVAL_REQUIRED`, nothing published. Exception: when every source is `current` and the plan writes nothing (docs/apply.md, step 4), the run prints `nothing to change: no approval needed` and `result: ok` and exits 0. A digest that is not the sync digest is `E_PLAN_STALE`, exit 3.
5. **Publication, under the writer lock.** A second copy, a second rehearsal and a second plan. The sync digest must still match, else `E_PLAN_STALE`, exit 3, nothing published. This rechecks the sources immediately before publication, and catches an upstream that moved between preview and approval, also a new commit over the same tree. Then, for each source in order, root first:
   - The checkout's HEAD must still be what the copy saw (`E_PLAN_STALE`).
   - The new commit is fetched from the staged copy into the checkout, and the checkout is fast-forwarded (`git merge --ff-only`). A nested checkout that does not exist is created with `git init` and `git checkout --detach`.
   - HEAD is read back as the postcondition. A checkout that is already at its commit prints `current` and is left alone.
6. **Apply.** The lock is released, and `apply` runs in process with the same allow flags and `--yes-plan <plan digest>`, the configuration plan digest the sync digest was built from. It plans `--root` (now the published tree), compares to the approved digest, takes the lock, plans again and applies (docs/apply.md). The receipt has `command = apply`.

Output: one `source:` line per source, then the plan when the run does not go on to apply, `published:` lines, then apply's own plan, report and `result:` line. The change in a `source:` line is one of:

- `<old>..<new>`
- `current at <commit>`
- `current at <head> (ahead of <commit>)` for a checkout ahead of its ref or pin
- `new checkout at <commit>`

followed by `(tracking)` or `(exact)`.

## Exit codes

| Code | When |
| --- | --- |
| 0 | Published (or nothing to publish) and applied, or nothing to change. A repeat sync with nothing new publishes nothing, applies nothing and writes no receipt. |
| 1 | After some publication: a later source failed to publish (`sync: published a; b was not published`), or apply refused or failed after the sources were published (`sync: published ...; the configuration was not applied`). Also apply's own exit 1 cases. |
| 2 | Usage, `--trust-origin` with `--preview` or with `--yes-plan`, a non-UTF-8 path. |
| 3 | Before any publication: untrusted origin, a fetch that failed (`E_SOURCE_MISSING`), a pin not in its ref (`E_SOURCE_MISSING`), a conflict in the rehearsal (`E_CONFLICT`), an invalid lock or composition, a blocked plan, a stale digest, a missing approval, the lock held. |
| 130, 143 | A signal during apply (docs/apply.md). The fetch and publication phases install no handler (see Known limits). |

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

## Decisions where the contract was silent

| Question | Choice |
| --- | --- |
| Where sources are declared | The `[[sources]]` of `dotfiles-manager.lock` at `--root`, in the contract's lock shape, plus `path` (where a nested checkout lives, default `.`). The binding's `origins` field stays unsupported. |
| Tracking versus exact | By position: the root source tracks (no `commit`), every nested source is exact (`commit` required). |
| Which lock names the nested sources | The fetched root revision's lock. The live lock's nested entries are ignored. The root entry must match between the two. |
| Trust without an interactive prompt | No prompt exists. `--trust-origin <origin>` is the explicit grant: it names the origin, is refused with `--preview`, and `--yes-plan` never grants. A grant may be written for an origin that no lock names yet. It has no effect until a lock names it. |
| Grant record | `state/grants.json` (state schema 2): `{origin, grant_id, granted_at}`, replaced atomically under the writer lock between a `grant-origin` journal `intent` (op id = the grant id, resource = the origin, no recovery outputs) and its `done` entry. The grants are read again under the lock before an origin is tested or added, so two concurrent `--trust-origin` runs cannot overwrite each other's grant (tested with the `sync-before-grant-lock` failpoint). A run that dies in between leaves the intent open, which `doctor`, `status` and `state recover` report. |
| Staging is a copy of the checkout | The plan digest covers the loaded source tree, which holds the user's dirty and untracked files. A staging tree built from the fetched commit alone would never match the tree after publication. The copy keeps them, so the rehearsal and the plan see exactly what publication produces, and git's own fast-forward rules decide conflicts the same way in both places. A socket or FIFO inside `.git` (a file monitor) is skipped; elsewhere it is `E_SOURCE_UNTRUSTED`. At most 1,000,000 entries. |
| Approval bound to commits | The sync digest covers each source's `(id, origin, ref, old, new)`, so a `--yes-plan` from a preview is stale after any new upstream commit. The second rehearsal under the lock recomputes it and also compares the per-source commits with the review pass. |
| A checkout ahead of its ref | Current, not moved back (DFM-70). The digest line has old = new = HEAD. |
| Two locks, one approval | The writer lock is held for the rehearsal and the publication, released, then taken again by `apply`. Another manager command can run between the two. `apply`'s digest comparison refuses a changed plan, and sync reports the published sources and exits 1. |
| Partial publication | Sources publish in lock order, root first. A failure after one source published is reported by name and exits 1. The checkouts already moved stay moved: a fast-forward is not undone. |
| Failed fetch, missing pin | `E_SOURCE_MISSING`, exit 3, before any plan. No fallback to the checkout's current content, and no digest is printed. A pin absent from its ref is refused with the head named and never substituted. |
| Read-only commands | `plan`, `doctor`, `status`, `explain` and `state check` read `--root` as it is and never fetch. A test keeps it so. |

## Known limits

- **Signals.** The gate test allows `signal::install` only in `apply_cmd.rs` and `trial_cmd.rs`, so sync's fetch, rehearsal and publication phases run with no handler. A SIGINT ends the manager and its git child together, because the child shares the manager's process group before the handlers exist. A stop during a fast-forward leaves what git left. Git updates refs last, so the checkout is at the old or the new commit, and the next sync rehearses again. The staging directory of a stopped run stays until the next approved run removes it. The kernel releases its lock.
- **Tree digest.** The lock's `tree` is not recomputed or compared (refused when present). Commit identity rests on git's object hashing with `transfer.fsckObjects=true`.
- **Copy cost.** Each approved run copies the whole checkout twice (review, then under the lock). A checkout with large ignored trees (`node_modules`, virtual environments) is slow to copy.
- **Credentials.** No credential helper from the binding (the binding's `origins` and helper fields are not implemented). `ssh` origins use the user's `~/.ssh` through the password database. The host-key policy is a device-gate item.
- **Receipt.** The receipt is apply's: `command = apply`, and `sources` lists the path-kind identity only. Sync prints the git commits that publication moved between, but state does not record them. The grant is in the journal and `grants.json`, not in a receipt (`origin_grants[]` waits for the sync receipt of DFM-31, a `shortcut` in `sync_cmd.rs`). `doctor` has no row for an unreceipted grant.
- **Nested checkout branches.** A new nested checkout is created detached at its pin. An existing one is fast-forwarded on whatever HEAD it has, so a branch moves with it.
- **Evidence scope.** Tests use the `test-isolation` build, a temporary HOME, `file://` bare remotes and `/usr/bin/git`. No test runs an `https` or `ssh` fetch or a GitHub origin. The DFM-70 commit message records a `dfm sync` run against the live `~/.dotfiles` on the personal M5 on 2026-10-24.
