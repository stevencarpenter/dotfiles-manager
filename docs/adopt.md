# Adopt

The `adopt` command (DFM-33): promote a trial into a module of the dotfiles repo. This document describes the behavior at HEAD. The trial record it reads is in [docs/trial.md](trial.md), the publish path it reuses in [docs/apply.md](apply.md), the design in [docs/superpowers/specs/2026-10-28-adopt-design.md](superpowers/specs/2026-10-28-adopt-design.md).

| Part | Location |
| --- | --- |
| `adopt` command: arguments, candidates, checks, digest, repo writes, takeover, receipt | `crates/cli/src/adopt_cmd.rs` |
| Secret scan (`gitleaks dir` over a private copy) and the checkout checks (`git status`, same checkout, not ignored) | `crates/cli/src/adopt_cmd.rs` (`scan`, `dirty`, `inside_git_tree`) |
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
   A link, a special file, an unreadable file or a file over 16 MiB refuses the run (`E_CONFLICT`). More than 64 files is `E_LIMIT_EXCEEDED`. No candidate is `E_CONFLICT` (`nothing to adopt`). Each file is read once: the read that decides whether a pre-existing file changed also supplies its bytes, and its mode, SHA-256 and fingerprint come from that read. The run under the lock reads each file once more, and the digest compare proves the bytes are the approved ones.
4. **Ownership and the checkout.** Each candidate gets the repo path `<manifest dir>/home/<HOME-relative path>`, the link source `home/<HOME-relative path>` and the resource id `resource_id(<HOME-relative path>)`. The run is refused (`E_CONFLICT`) when:
   - two candidates give the same resource id (`a.b` and `a/b`);
   - a resolved resource of any module claims the target, or a directory above or below it;
   - the id already exists in any module;
   - an active trial other than this one claims the live path in its `owned_outputs`;
   - the repo path exists, tracked or not;
   - the manifest or the Brewfile has uncommitted changes (`git status`);
   - a component on the way to a repo path, the manifest or the Brewfile is a link, or the deepest existing one is not writable.
5. **The Brewfile.** Only for a `homebrew/v1` trial. The file is `<root>/Brewfile.<module>` when it exists, else `<root>/Brewfile`. A missing file is `E_CONFLICT` with its path. It must be inside the checkout and not ignored. The line is `brew "<name>"`, or `cask "<name>"` when brew holds the package as a cask (Caskroom rack and no Cellar rack, or a `homebrew/cask/` name). The name is the recorded package without an official `homebrew/core/` or `homebrew/cask/` prefix. A third-party tap keeps `owner/tap/name`, because its short name would install another formula, or none, on a fresh machine. A version suffix (`node@20`) is kept. A line of either kind for any spelling of the same package (`tuicr`, `homebrew/core/tuicr`) means nothing is appended. A `mise/v1` trial, or a trial with no provider package, gets no line, and the preview says why.
6. **The scan.** `gitleaks dir` over a manager-owned copy of the candidates (see "The secret scan"). A finding is `E_CONFLICT` and names the live path, never the value. A missing `gitleaks` or a scan that does not complete is `E_PROBE_FAILED`.
7. **The edited composition.** The manifest with the appended blocks and the repo copies are put into the loaded sources in memory. The profile is resolved again: a resolve error refuses the run. Each new resource must resolve as `<module>.<id>`, declared in that manifest, with the candidate as its target. The configuration plan of that composition, with the candidates approved for takeover, must hold exactly one planned `adopt-file` operation per new resource, with no blocker naming one.
8. **Preview.** `adopt:`, `manifest:`, one `candidate:` line per file (live path, repo path, id), `brewfile:`, `takeover:`, the record's `unknown:` tokens and `plan digest:`. `--preview` stops here, exit 0 (3 when refused). Without `--yes-plan`, the run stops with `E_APPROVAL_REQUIRED` (exit 3) and the digest to pass. The digest is the SHA-256 of the canonical JSON of: trial id, the SHA-256 of the trial record's canonical JSON, module, profile path, manifest path and SHA-256, Brewfile path and SHA-256 (or none), the Brewfile line (or none), each candidate (live path, size, mode, SHA-256, repo path, id), the binary identity and the binding stamp.
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
| 3 | Refused before any write: approval missing, stale digest, a refusal in steps 1 to 7, lock held, not bound. |
| 130, 143 | A stop request. A child (`gitleaks`, `git`, `brew`) is ended. |

## The secret scan

`gitleaks dir` reads `.gitleaks.toml` and `.gitleaksignore` from the scanned directory, and a planted config could allowlist everything. Adopt therefore scans a copy under `<cache dir>/adopt-scan/<pid>-<ns>/` (0700, files 0600, removed on every exit but SIGKILL):

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
| How the appends are written | Through one `O_NOFOLLOW` descriptor that is read, checked against the digest's SHA-256 and appended to. The engine's atomic replace writes mode 0600 and opens its directory by a resolved path, so it is not used for a file the owner keeps at its own mode. An interrupted append leaves the old bytes followed by a prefix of the planned text, which the token describes. |
| Which part of the plan runs | Only the new resources' `adopt-file` operations. Unrelated drift and blockers elsewhere in the composition are left for `apply` and `sync`. |
| `apply` and `sync` | Unchanged. Without `adopt`'s approval a regular file at a link target stays `E_CONFLICT` (docs/planner.md, docs/apply.md). After adopt, before the commit, `apply --preview` plans the link `current`. |

## Known limits

- **Parent links between the check and the write.** The walk that refuses a link on the way to a repo path, the manifest or the Brewfile runs under the lock, before the writes. A directory swapped for a link after that walk is not detected; only the final component is opened without following a link.
- **Writable check.** Only the owner bits (for the owner) and the other bits are read; group permissions are not.
- **Record size.** The intent token is at most 4096 bytes. A run whose record is larger is refused before any write (`E_LIMIT_EXCEEDED`).
- **Interruption.** A second run after a stop does not yet recognise its own partial work (DFM-33 Task 6): a repo copy or a dirty manifest from the first run refuses it.
- **Evidence scope.** Tests use the `test-isolation` build, a `gitleaks` look-alike and the `brew` look-alike. The real `gitleaks` is tested only by the scan unit test, and only where it is on `PATH`. `crates/cli/tests/sync.rs` runs `sync --preview` after adopt and before the commit; no test runs `sync` with a refused equal regular file.
