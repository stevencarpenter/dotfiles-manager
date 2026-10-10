# Bootstrap, binding and build controls

Task: DFM-14. This document records what the first implementation does and where it differs from [docs/architecture.md](architecture.md) (the contract). The contract wins where this document is silent.

## What exists

| Part | Location |
| --- | --- |
| Workspace crates `dfm-model` (pure data and rules), `dfm-platform-macos` (HOME, paths, platform check), `dfm-cli` (binary `dotfiles-manager`) | `crates/` |
| POSIX sh bootstrap and build-controls checker | `scripts/bootstrap.sh` |
| Shared checker fixtures (102 cases) | `tests/fixtures/build-controls/` |

`ports`, `adapters` and `engine` are not created yet. No task needs them.

## CLI

```
dotfiles-manager --version | --help
dotfiles-manager bind --profile <file> --machine-id <id>
dotfiles-manager binding
dotfiles-manager self-check --build-controls <staged tree> | --fixture-dir <dir>
```

- `bind` records the canonical profile path and a machine identity in `~/.config/dotfiles-manager/machine.toml` (schema 1, mode 0600, directory 0700). Nothing is inferred: both flags are required.
- A second `bind` with the same identity succeeds without a write. A different profile or machine id exits 3 with `E_REBIND_UNSUPPORTED`. There is no force flag. Rebinding is an ownership migration and is not implemented.
- The binding file is published with a temporary file, `fsync` and `hard_link`, which never replaces an existing file. Concurrent binds leave one identity.
- Nothing applies configuration. Installation and `bind` only write the files named here.
- Errors are one line, at most 512 characters, with control characters escaped. Exit codes follow the contract (0, 1, 2, 3, 4, 5, 130, 143). Codes 4, 5, 130 and 143 are defined and tested for precedence but no command produces them yet. The CLI installs no signal handler.
- Isolation overrides (`--home`, `DFM_HOME`, `DFM_BINDING`, `DFM_PLATFORM`) compile only with the cargo feature `test-isolation`. Those builds print `isolated: true` and append `test-isolation` to `--version`. The installed release binary ignores them (tested). The feature is switched on only by `--features dfm-cli/test-isolation` (the gate does this); no self dev-dependency enables it, and a gate test builds every release target without the flag and checks `--version`. `rust-toolchain.toml` in the repository lists `rustfmt` and `clippy`; the checker allows exactly those two components besides the pinned channel.

## Source and build identity

- A source identity is the git commit plus the recomputed tree digest. `bootstrap.sh install` requires `--commit <40 hex>` and refuses unless `HEAD` equals it. Git runs with the scrubbed environment from the contract. The script runs no `git status`, because that would run filters configured in the repository (tested). Uncommitted files are ignored: only `HEAD` is built.
- The build input is `git archive <commit>` of the verified `--commit` (not whatever `HEAD` is). Git runs with `GIT_NO_REPLACE_OBJECTS=1` and `core.useReplaceRefs=false`, so a `refs/replace` entry cannot swap the content (tested). `--commit` must be a full lowercase hex object id (40 digits, or 64 in a sha256 repository whose object format matches). Abbreviations, uppercase, refs, tags, `HEAD`, `^`, `~`, `@{}`, option-like values and whitespace are refused with `E_SOURCE_UNTRUSTED` before any git call (tested for 19 forms). git resolves `<id>^{commit}` after `--end-of-options`, and the resolved id must equal the pin. The pin is not read from the lock: this implementation's lock carries the toolchain fields only. The script reads an existing checkout; it does not fetch into a staging directory with `transfer.fsckObjects`, which the contract describes for fetches.

- The script sets `DFM_BUILD_COMMIT` for the build. `--version` prints `dotfiles-manager <semver> (commit <sha>)`. The installed directory is `versions/<semver>-<first 12 of commit>`.
- The binary SHA-256 is printed after install as a record only. Builds are not claimed to be reproducible.
- The owner obtains the script and commit out of band, as in contract section 12 item 3.1. This implementation does **not** clone or verify the script checksum itself: the owner clones the pinned commit with a scrubbed Git environment, checks `git rev-parse HEAD` and the script SHA-256, then runs the script from that checkout.

## Running the script

Run it as `/usr/bin/env -i /bin/sh scripts/bootstrap.sh <command> ...`. The script also re-executes itself under `env -i` with a fixed `PATH`, so exported shell functions (`BASH_FUNC_*`), `PERL5LIB`/`PERL5OPT` (`shasum` is perl), `CDPATH` and `ENV` do not change what is checked (tested). Relative `--source` and `--dir` values are made absolute first. A caller's `SHELLOPTS=noexec` makes `/bin/sh` skip the whole file and exit 0; no script can prevent that, so a successful run is one that prints its final line (`bootstrap: done` for install).

## Install, upgrade, rollback, removal

1. Place the exact pinned toolchain at `~/.local/share/dotfiles-manager/toolchains/<version>/`. Compute its digest with `sh scripts/bootstrap.sh toolchain-digest --dir <that dir>`. Review it, then write `[manager] toolchain` and `toolchain_tree_sha256` into `dotfiles-manager.lock` in the commit to build. The repository does not carry a lock yet, because the digest of the toolchain on each device is not known.
2. `sh scripts/bootstrap.sh install --source <checkout> --commit <sha>`. Order: platform, HOME, source identity, toolchain presence, Command Line Tools (polled up to 30 minutes), stage, build controls, then `cargo fetch --locked`, `cargo build --release --locked --offline` with an empty constructed environment, `self-check --fixture-dir`, and install. No cargo, rustc or rustup process starts before the controls pass.
3. Upgrade is the same command with a newer commit. The new version directory is written first. `current` flips by one atomic rename of a symlink, after the new binary passed `self-check`. A failed build or check leaves the previous version active. Old version directories stay. State is not migrated here (state upgrade belongs to a later task).
4. Rollback: point `current` at the previous `versions/<name>` (`ln -s versions/<name> current.new && mv -fh current.new current`).
5. `sh scripts/bootstrap.sh uninstall` removes the PATH link, `versions/`, `current`, the build directories and the cache. It leaves toolchains, state and the binding.

The running manager is never replaced in place, so an upgrade does not depend on the executable it replaces. The install needs only POSIX sh, system tools in `/usr/bin:/bin:/usr/sbin:/sbin`, Git, Command Line Tools and the placed toolchain.

## Build controls and the restricted grammar

`bootstrap.sh check-controls --source <tree>` and `dotfiles-manager self-check --build-controls <tree>` implement the same checks: staged tree, then toolchain tree digest, then ancestors. Both exit 3 on refusal and print the first failing control. The differential test runs both over `tests/fixtures/build-controls/cases/*` and requires the same result.

The checker reads only: comments; `[a.b]` and `[[a.b]]` headers with bare names; `key = value` with a bare key and a single-line string without escapes, boolean, integer, array or inline table of those. It refuses everything else, including multi-line values, dotted or quoted keys, floats and dates, non-ASCII bytes, CR, and files over 1 MiB.

Rules: names are matched case-insensitively, and two paths that differ only in case are refused (the macOS file system folds case); duplicate tables, keys and inline keys are refused; no `.cargo/config.toml` or `.cargo/config` at any level; no `rust-toolchain` file; every `rust-toolchain.toml` must contain only `channel` equal to the lock pin; every `path`, `members`, `exclude`, `default-members`, `build` and `workspace` string must stay inside the root (a `..` after a normal component is refused); `git`, `registry` and unlisted dependency keys are refused; `[replace]` is refused; `[patch]` is allowed only for `crates-io`; `Cargo.lock` lines must be recognised and any `source` must be crates.io; symlinks and special files in the tree are refused; the root `Cargo.toml` must declare `[workspace]` (otherwise a `Cargo.toml` planted in an ancestor could adopt it and add `[patch]` tables); an ancestor of the build directory that cannot be searched is refused with `E_BUILD_CONFIG` instead of being treated as free of config.

The checker and `toolchain-digest` fail closed: an unreadable entry, a failed hash, a bad symlink or an empty toolchain is a refusal, never a digest of the readable part. After the build the script requires the binary to report exactly `dotfiles-manager <digits.dots> (commit <the verified commit>)` before it names the install directory.

## Differences from the contract

1. **Staged-tree refusals use `E_SOURCE_UNTRUSTED`.** Contract section 8 item 3 says so for a staged `.cargo/config*`, but section 8 item 3a lists "configuration" under `E_BUILD_CONFIG`. This implementation follows item 3: `E_BUILD_CONFIG` is for the build directory and its ancestors only.
2. **Stricter than the contract:** any symlink in the staged tree is refused (the contract allows symlinks that stay inside the root); `[replace]` is refused outright; Cargo manifests may not use multi-line arrays or strings.
3. **Binding location.** The binding is at `~/.config/dotfiles-manager/machine.toml` as contract section 5 says, not in the state directory. Durable binding storage in state belongs to DFM-17 (C07 is not claimed).
4. **New error codes** beyond the contract list: `E_PLATFORM_UNSUPPORTED`, `E_REBIND_UNSUPPORTED`, `E_BINDING_INVALID`, `E_PROFILE_INVALID`, `E_USAGE`, `E_PREREQ_MISSING` (script only).
5. **Tree digest framing.** Each entry is `path NUL class NUL value LF`; the contract does not fix the terminator. The class `exec` means the owner execute bit is set.
6. **Test hooks in the script.** With `DFM_BOOTSTRAP_TEST=1` the script reads `DFM_TEST_HOME`, `DFM_TEST_OS` and `DFM_TEST_ARCH` in place of `dscl` and `uname`. They exist so tests can use a temporary HOME. A caller who can set them already controls the user's environment; they are not an integrity boundary. Without the variable the script uses the password database, and a temporary HOME gives `E_HOME_MISMATCH` (tested).
7. **Cache location.** `cache_dir()` is `~/.cache/dotfiles-manager`; the contract leaves it open. The script and `dfm-platform-macos` hard-code the same literal and a test compares them.
8. **Signals.** The CLI does not catch SIGINT or SIGTERM; no command yet runs long enough to need a receipt.
9. **Fetch.** `cargo fetch --locked` runs with crates.io as the only registry in an empty `CARGO_HOME`. The install test therefore needs network access.

## Not verified

Device behavior on the personal M5 and work M3 (DG-1, C09): Cargo configuration discovery with the pinned Cargo, whether the empty build environment suffices on each device's Command Line Tools, ad-hoc signing under device policy, the Command Line Tools install poll, and the real toolchain digest. The install test used a minimal copy of the authoring machine's toolchain, not an official release distribution install. The channel manifest digest is carried in the lock format but not checked by this script (toolchain install is a later typed operation).

The differential test cannot place two paths that differ only in case on this file system, so the case-collision rule is covered by a unit test in `dfm-model` and by manual reasoning for the awk version only.
