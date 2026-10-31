# Adopt

The `adopt` command (DFM-33): promote a trial into a module of the dotfiles repo. This document describes the behavior at HEAD. The trial record it reads is in [docs/trial.md](trial.md), the publish path it reuses in [docs/apply.md](apply.md), the design in [docs/superpowers/specs/2026-10-28-adopt-design.md](superpowers/specs/2026-10-28-adopt-design.md).

| Part | Location |
| --- | --- |
| `adopt` command: arguments, candidates, checks, digest, repo writes, takeover, receipt | `crates/cli/src/adopt_cmd.rs` |
| Secret scan (`gitleaks dir` over a private copy) and the checkout checks (`git status`, same checkout, not ignored) | `crates/cli/src/adopt_cmd.rs` (`scan`, `dirty`, `inside_git_tree`, `check_ignore`) |
| Resource id, link block, append, Brewfile line | `crates/model/src/adopt.rs` |
| Module manifest lookup | `crates/model/src/resolve/scope.rs` (`module_manifest`) |
| The takeover operation (`adopt-file`), planned only from `adopt`'s approval | `crates/model/src/plan/build.rs` (`Context.adopt_files`), `crates/engine/src/apply.rs` |
| Ending the trial | `crates/engine/src/store.rs` (`mark_trial_adopted`) |
| Tests with `brew` and `gitleaks` look-alikes and the system `git` | `crates/cli/tests/adopt.rs` |

## The command

```
dfm adopt <trial-id> --module <module> [--root <dir>] [--preview] [-y|--yes-plan <digest>]
```

- `<trial-id>` names a record in `state/trials/`, for example `trial-tuicr`.
- `--module` is required. Adopt never creates a module, never edits the profile and never merges modules.
- `--root` defaults to the root the binding records, as for `apply`. The machine must be bound.
- Adopt never commits or pushes. The output ends with the repo paths it changed, for `git diff` and `git status`.

Order of one run:

1. **The trial.** The signal handlers go in. The trial record is read. A missing or unreadable record is `E_STATE_INVALID`. An adopted record is `E_CONFLICT`.
2. **The module.** The bound profile is resolved. A module the bundle chain does not declare is `E_REFERENCE_INVALID`, with the profile path. A declared module the profile does not select is `E_REFERENCE_INVALID`, with the profile path and its `modules = [...]` line to edit. The manifest path comes from the bundle chain (`module_manifest`) and must be a regular file the resolver loaded. It must be inside the `--root` git checkout and not ignored by git.
3. **The candidates.** For each `baseline.paths` entry of the record, as it exists now:
   - `existed: false`: every regular file now under the path (or the path itself, when it is a file).
   - `existed: true` with a `sha256`: the file, only when its `file_fingerprint` changed. An unchanged pre-existing file is not the trial's work.
   - `existed: true` without a hash (a directory that existed before): every regular file under it, marked `(pre-existing directory)` in the preview.
   A link, a special file, an unreadable file or a file over 16 MiB (the read cap) refuses the run (`E_CONFLICT`). More than 64 files is `E_LIMIT_EXCEEDED`. The repo copy must also pass the source loader (`crates/cli/src/source_fs.rs`), which reads it in every later `plan` and `sync`: a file over 1 MiB (`MAX_SOURCE_FILE_BYTES`, 1048576 bytes) is `E_LIMIT_EXCEEDED`, and a repo path with a `.git` component (a nested repository) is `E_SOURCE_UNTRUSTED`. A name with a control character is refused by the resolve in step 7 (`E_TARGET_INVALID`). No candidate is `E_CONFLICT` (`nothing to adopt`). Each file is read once: the read that decides whether a pre-existing file changed also supplies its bytes, and its mode, SHA-256 and fingerprint come from that read. The run under the lock reads each file once more, and the digest compare proves the bytes are the approved ones.
4. **Ownership and the checkout.** Each candidate gets the repo path `<manifest dir>/home/<HOME-relative path>`, the link source `home/<HOME-relative path>` and the resource id `resource_id(<HOME-relative path>)`. The run is refused (`E_CONFLICT`) when:
   - two candidates give the same resource id (`a.b` and `a/b`);
   - a resolved resource of any module claims the target, or a directory above or below it;
   - the ledger records the target, or a path above or below it, for any resource, also one that no module declares now (an orphan). The only exception is a kept link of an interrupted run, recorded for its own new resource;
   - the id already exists in any module;
   - an active trial other than this one claims the live path in its `owned_outputs`;
   - the repo path exists, tracked or not, unless it is the copy of an interrupted run (see "Interruption");
   - git ignores the repo path (`git check-ignore`), a kept copy of an interrupted run included. `git add` would skip the copy, and a fresh checkout could not resolve the new resource. Git runs in the scrubbed environment of `sync` (no global or system config), so adopt passes the user's global excludes file with `-c core.excludesFile=<path>`: the `core.excludesFile` value of `~/.gitconfig`, else of `$XDG_CONFIG_HOME/git/config` (default `~/.config/git/config`), each read alone with `git config --file`, else `$XDG_CONFIG_HOME/git/ignore`. A leading `~/` is expanded to HOME. An empty value means no excludes file, as in git (no fallback to the XDG default). When the repository's own config sets `core.excludesFile` (`git -C <root> config --get`, which in the scrubbed environment reads only the repository and worktree config), no `-c` is passed and git uses that value, as the user's `git add` does. The manifest and Brewfile checks use the same file;
   - a repo path, or a directory on its way, differs only by case from a loaded source path or directory, or from another candidate (`E_SOURCE_UNTRUSTED`, as the source loader refuses);
   - the manifest or the Brewfile has uncommitted changes (`git status`), unless an interrupted run already appended to it;
   - a component on the way to a repo path, the manifest or the Brewfile is a link, or the deepest existing one is not writable.
5. **The Brewfile.** Only for a `homebrew/v1` trial. The file is `<root>/Brewfile.<module>` when it exists, else `<root>/Brewfile`. A missing file is `E_CONFLICT` with its path. It must be inside the checkout and not ignored. The line is `brew "<name>"`, or `cask "<name>"` when brew holds the package as a cask (Caskroom rack and no Cellar rack, or a `homebrew/cask/` name). That probe runs only when the kind decides something: no line of either kind exists, or a line of only one kind does. When that probe fails and a line exists, the line stays and nothing is appended; with no line, the failure is `E_PROBE_FAILED`. The name is the recorded package without an official `homebrew/core/` or `homebrew/cask/` prefix. A third-party tap keeps `owner/tap/name`, because its short name would install another formula, or none, on a fresh machine. A version suffix (`node@20`) is kept. A line of the same kind (`brew` for a formula, `cask` for a cask), plain or in the paren form (`brew("tuicr")`), for any spelling of the same package (`tuicr`, `homebrew/core/tuicr`) means nothing is appended. A formula and a cask of one name are different packages: `brew "docker"` does not count for `cask "docker"`, and the reverse. A `brew "x"` line under `tap "owner/tap"` does not count for `owner/tap/x`, because brew resolves a short name to `homebrew/core` first; the qualified line is appended. A `mise/v1` trial, or a trial with no provider package, gets no line, and the preview says why.
6. **The scan.** `gitleaks dir` over a manager-owned copy of the candidates (see "The secret scan"). A finding is `E_CONFLICT` and names the live path, never the value. A missing `gitleaks` or a scan that does not complete is `E_PROBE_FAILED`.
7. **The edited composition.** The manifest with the appended blocks and the repo copies are put into the loaded sources in memory. The composition must stay within the source loader's tree limits: at most 20000 entries (files, links and directories, the new parent directories of the repo copies included), at most 64 MiB of source bytes in total, and at most 1 MiB for the appended manifest and, when the loader reads it, the appended Brewfile (`E_LIMIT_EXCEEDED`). The profile is resolved again: a resolve error refuses the run. Each new resource must resolve as `<module>.<id>`, declared in that manifest, with the candidate as its target. The configuration plan of that composition, with the candidates approved for takeover, must hold exactly one planned `adopt-file` operation per new resource, with no blocker naming one.
8. **Preview.** `adopt:`, `resume:` after an interruption, `manifest:`, one `candidate:` line per file (live path, repo path, id), `brewfile:`, `takeover:`, the record's `unknown:` tokens and `plan digest:`. `--preview` stops here, exit 0 (3 when refused). Without `--yes-plan`, the run stops with `E_APPROVAL_REQUIRED` (exit 3) and the digest to pass. The digest is the SHA-256 of the canonical JSON of: trial id, the SHA-256 of the trial record's canonical JSON, module, profile path, manifest path and SHA-256, Brewfile path and SHA-256 (or none), the Brewfile line (or none), each candidate (live path, size, mode, SHA-256, repo path, id, and whether its copy and its link are kept from an interrupted run), whether each append is kept, the interrupted run's op id (or none), the binary identity and the binding stamp. The SHA-256 of an appended file is the one before the append.
9. **Under the lock.** The writer lock is taken, the identity and the ledger are checked, and steps 1 to 5 and 7 run again. The scan does not run again: the digest covers the hash of every scanned byte. A different digest is `E_PLAN_STALE` (exit 3, nothing written).
10. **The intent.** The journal gets a `trial-adopt` intent (resource: the trial id). The name is not `adopt`, which is the engine's link adoption that recovery probes by content. Its recovery list is empty. Its `token` is a JSON record of the run: the module, the manifest and the Brewfile, each with its path and SHA-256 before and after the append, and each file with its target, repo path and SHA-256. A later run uses it to recognise its own partial work by content.
11. **The repo writes.** In order, each followed by a failure point:
    - each repo copy, created exclusively (`O_CREAT|O_EXCL`, no link followed) with the live file's permission bits, and missing parent directories created through real directories only (`adopt-after-copy`);
    - the manifest append (`adopt-after-manifest`);
    - the Brewfile append, when there is a line (`adopt-after-brewfile`).
    An append opens the file once without following a link, reads it from that descriptor, requires the SHA-256 recorded in the digest, and appends through the same descriptor. A changed file is `E_PLAN_STALE` and is not edited. Then the `trial-adopt` entry is closed `done` (or `failed`).
12. **The takeover.** The profile is resolved from the root on disk and planned with the candidates approved for takeover. The plan is cut to the new resources' operations, which must again be one planned `adopt-file` each, and the engine applies it with those ids as its adopt approvals. The engine journals each takeover as its own `adopt-file` operation, rechecks the live file and the source under the lock, replaces the file with the link and records the ledger owner. Failure point `adopt-after-link`.
13. **The end.** The trial record is marked adopted (schema 3, `adopted{op_id, module, at_unix_ns}`). The receipt has `command = adopt`, the digest and a `yes_plan` approval, one operation per written repo path (`before` and `after` SHA-256 of an appended file, `after` of a new copy), the engine's `adopt-file` operations with their `allow_adopt` approvals, and one operation for the trial. The output ends with one `changed:` line per repo path, the trial line, `receipt:` and `result:`.

| Exit | When |
| --- | --- |
| 0 | Adopted: the repo files, the links and the ended trial. |
| 1 | A failure after the first journal entry. The repo writes that happened stay; the receipt lists them. |
| 2 | Usage. A `--root` that does not exist (`E_REPORT_UNAVAILABLE`, as for `plan` and `apply`). |
| 3 | Refused before any write: approval missing, stale digest, a refusal in steps 1 to 7, lock held, not bound, a cache or root path that is not UTF-8. |
| 130, 143 | A stop request. A child (`gitleaks`, `git`, `brew`) is ended. |

## Interruption

The owner runs the same command again after a stop (exit 1, 130, 143, or a killed process). The run completes the adoption; it never deletes a path and never writes one twice.

- **The record.** The latest `trial-adopt` intent of the trial, in any phase (open, `done`, `failed`, `unknown`), while the trial record is active. An intent older than the trial record, or for another module, is ignored. A token that cannot be read is `E_STATE_INVALID`.
- **What counts as its own work.** Each check is by content against the token:
  - a repo copy whose bytes hash to the recorded SHA-256, which is also the candidate's SHA-256 now;
  - a manifest or Brewfile whose bytes hash to the recorded `post`, and are bytes hashing to the recorded `pre` followed by exactly the planned append;
  - a live path that is a link to the recorded repo copy. Its candidate bytes and mode are read from that copy.
- **Anything else** is `E_CONFLICT` with the path, and nothing is written:
  - a repo copy that is not the recorded one, a torn copy included (fix: compare it with the live file, and remove a partial copy);
  - a live file changed after the copy (fix: restore the copied bytes, or remove the copy to adopt the new bytes);
  - a repo copy changed through the live link after the takeover; the message says so (fix: commit or revert that change in the copy);
  - a manifest or Brewfile changed after the stop, a torn append included (fix: revert only what changed after the stop, then run again). A torn manifest append that no longer parses is refused by the resolver first, with `E_MANIFEST_INVALID` and the manifest path;
  - a live link to anything but the recorded repo copy.
- **The checks of a fresh run.** They run against the composition before the kept manifest append, so the planned resources do not collide with themselves. A kept file is not checked for uncommitted changes or for writability, because it is not written. The walk that refuses a link on the way runs for every repo path, kept ones included, before a kept copy is accepted.
- **The takeover.** A kept link that the ledger owns is planned `current` and needs no approval. A kept link without an owner (for example after `dfm state recover` closed the engine's `adopt-file` intent) is a planned `adopt`; its id is approved only because the live link equals the recorded one. The engine then records the owner.
- **The superseded intents.** The run that completes the adoption closes the open `trial-adopt` intent of each earlier run of this trial and module (also one that a later `apply` or `state recover` left `unknown`) with `failed` and the reason `superseded by <op_id>`, just before its own `done`. These intents name no output path, so no probe could settle them. The close appends entries only; it claims no path and runs nothing again. An intent for another module, or one whose token cannot be read, stays open, and `state check` and `doctor` report it as `E_OP_INTERRUPTED` until `state resolve <journal-id> --as failed`. For an open or `unknown` `trial-adopt` entry, the `doctor` fix names `dfm adopt <trial> --module <module>` (the module from the token) and `dfm state resolve '<journal-id>' --as failed`, not `apply` or `state recover`, because no path names the entry.
- **The preview and the digest.** `resume:` names the interrupted op id, and each kept item says `kept from the interrupted run`. The digest covers what is kept, so the approval matches what is left to do.
- **Concurrent runs.** The second run's preview reads the first run's partial work. Its approval needs the writer lock and is refused with `E_LOCKED` while the first run holds it, before any write.

## The secret scan

`gitleaks dir` reads `.gitleaks.toml` and `.gitleaksignore` from the scanned directory, and a planted config could allowlist everything. Adopt therefore scans a copy under `<cache dir>/adopt-scan/<pid>-<ns>/` (0700, files 0600, removed on every exit but SIGKILL). Each scan holds an exclusive `flock` on its own `lock` file, which the kernel releases when the process ends. Before each scan, under a lock on `adopt-scan/`, a `<pid>-<ns>` directory of the current user whose `lock` is free is removed, so a killed run leaves its copy only until the next scan. A directory without a `lock` (a scan killed before it locked, or a running scan of an older binary during a self-update) is removed only once its `<ns>` is older than the 120 s scan timeout. No signal is sent, and a reused pid does not matter. The lock on `adopt-scan/` is tried every 10 ms for at most 10 s, never waited for in `flock`: a stop request ends the wait with exit 130 or 143, and a lock still held is `E_LOCKED`. A scan directory of another user is `E_CONFLICT`, and a report larger than 16 MiB is `E_PROBE_FAILED`:

- Each candidate is written twice in `files/<i>/`: as `<i>`, so the default path allowlist cannot skip its content, and under its base name, so name rules (`cert.p12`) still apply.
- No `--config` is passed (an empty one disables the default rules), the runner clears the environment (no `GITLEAKS_CONFIG`), and `--gitleaks-ignore-path` names an empty directory.
- `--redact` and `--ignore-gitleaks-allow` are set. Only the `File` field of the report is read.

The dotfiles commit hook (`gitleaks git --staged`) stays as the second check.

## Decisions

| Question | Choice |
| --- | --- |
| Files or directories | Files only. A directory link would also capture what the tool writes later. |
| Where the journal record lives | In the intent's `token`. A recovery output path must be inside HOME, and the repo may not be; an output path would also let recovery probe the entry by content. The op name is `trial-adopt`, distinct from the engine's `adopt` and `adopt-file` arms in recovery. |
| When the `trial-adopt` entry closes | After the repo writes, before the takeover. The engine settles every open intent before it applies, so an open `trial-adopt` intent would be reconciled (as `unknown`) in the middle of the run. The takeover is journaled per link by the engine. |
| How the appends are written | Through one `O_NOFOLLOW` descriptor that is read, checked against the digest's SHA-256 and appended to. The engine's atomic replace writes mode 0600 and opens its directory by a resolved path, so it is not used for a file the owner keeps at its own mode. A stop inside an append leaves the old bytes followed by a prefix of the planned text. That prefix is not the recorded append, so a later run refuses it (see "Interruption"). |
| Which part of the plan runs | Only the new resources' `adopt-file` operations. Unrelated drift and blockers elsewhere in the composition are left for `apply` and `sync`. |
| `apply` and `sync` | Unchanged. Without `adopt`'s approval a regular file at a link target stays `E_CONFLICT` (docs/planner.md, docs/apply.md). After adopt, before the commit, `apply --preview` plans the link `current`. |

## Known limits

- **Parent links between the check and the write.** The walk that refuses a link on the way to a repo path, the manifest or the Brewfile runs under the lock, before the writes. A directory swapped for a link after that walk is not detected; only the final component is opened without following a link.
- **Global excludes.** An `[include]` or `[includeIf]` in the user's git config is not followed, and `~user/` in `core.excludesFile` is not expanded. A rule that only such a file sets is not seen, and `git add` may still skip the copy.
- **Writable check.** Only the owner bits (for the owner) and the other bits are read; group permissions are not.
- **Record size.** The intent token is at most 4096 bytes. A run whose record is larger is refused before any write (`E_LIMIT_EXCEEDED`).
- **Interruption.** A kept manifest or Brewfile append is matched to the bytes the interrupted run found, not to the committed file. A commit made after the interruption that leaves those bytes unchanged is not detected.
- **Evidence scope.** Tests use the `test-isolation` build, a `gitleaks` look-alike and the `brew` look-alike. The real `gitleaks` is tested only by the scan unit test, and only where it is on `PATH`. `crates/cli/tests/sync.rs` runs `sync --preview` after adopt and before the commit; no test runs `sync` with a refused equal regular file.
