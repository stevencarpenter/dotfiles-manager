#!/usr/bin/env bash
# The single shared gate: pre-commit hook, `just check` and hosted CI all run this.
# Usage: check_all.sh [step ...]
#   steps: fixtures scripts rust-style rust-tests network-tests (default: the first four;
#   rust-tests runs network-tests itself unless DFM_SKIP_NETWORK_TESTS=1), and rust-archive,
#   which only hosted CI runs (it needs cargo-nextest)
set -euo pipefail

# Documentation fixtures only: shape and references. The resolver cases run in step rust-tests.
step_fixtures() {
    uv run scripts/check_examples.py
}

# Python helper style, shell syntax and the staged-gate script test.
step_scripts() {
    uvx --python 3.14 ruff@0.14.14 check scripts
    uvx --python 3.14 ruff@0.14.14 format --check scripts
    bash_scripts=(scripts/check_all.sh scripts/check_staged.sh scripts/check_push.sh examples/external/modules/work/files/session-hook.sh)
    # `bash -n a b c` parses only the first file, so each file gets its own run.
    for f in "${bash_scripts[@]}"; do
        bash -n "$f"
    done
    # bootstrap.sh is POSIX sh by design (docs/bootstrap.md): `sh -n` checks syntax only.
    sh -n scripts/bootstrap.sh
    if command -v shellcheck >/dev/null 2>&1; then
        shellcheck -s sh -S warning scripts/bootstrap.sh
        shellcheck -s bash -S warning "${bash_scripts[@]}"
    elif [ "${DFM_SKIP_SHELLCHECK:-}" = 1 ]; then
        echo "SKIPPED: shellcheck (DFM_SKIP_SHELLCHECK=1)"
    else
        echo "error: shellcheck is not installed; install it or set DFM_SKIP_SHELLCHECK=1" >&2
        return 1
    fi
    uv run --python 3.14 scripts/test_check_staged.py
}

# Formatting and static analysis of the Rust workspace.
step_rust_style() {
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    # The second run lints the code and tests that only exist in the isolated build.
    cargo clippy --workspace --all-targets --locked --features dfm-cli/test-isolation -- -D warnings
}

# The test set of the gate: the whole workspace in the isolated build. The CLI and differential
# tests need feature test-isolation, which also switches on dfm-engine/failpoints.
test_select=(--workspace --locked --features dfm-cli/test-isolation)

# Hosted CI only: build every test binary of the test set once into a cargo-nextest archive
# (DFM_TEST_ARCHIVE), so each test job runs prebuilt binaries. nextest cannot archive doc tests,
# so they run here, once, against the libraries the archive build already compiled.
step_rust_archive() {
    cargo nextest archive "${test_select[@]}" --archive-file "${DFM_TEST_ARCHIVE:?set DFM_TEST_ARCHIVE}"
    cargo test --doc "${test_select[@]}"
}

# Runs a selection of the archive. The archive is extracted into this checkout, so the build
# paths compiled into the tests (CARGO_BIN_EXE_dfm, CARGO_MANIFEST_DIR) name real files: CI
# builds and runs in the same checkout path.
run_archive() {
    cargo nextest run --archive-file "$DFM_TEST_ARCHIVE" --workspace-remap . \
        --extract-to . --extract-overwrite --partition "${DFM_TEST_PARTITION:?set DFM_TEST_PARTITION}" "$@"
}

# Implementation tests: unit, CLI, bootstrap script and build-controls differential tests.
# Locally `cargo test` runs the test set. With DFM_TEST_ARCHIVE set (hosted CI), the step runs
# the nextest partition DFM_TEST_PARTITION (for example count:2/4) of that prebuilt archive
# instead; each test then runs in its own process.
# Set DFM_SKIP_NETWORK_TESTS=1 (exactly 1) to skip the network-tests step; the skip is printed.
step_rust_tests() {
    if [ -n "${DFM_TEST_ARCHIVE:-}" ]; then
        run_archive
    else
        cargo test "${test_select[@]}"
    fi
    if [ "${DFM_SKIP_NETWORK_TESTS:-}" = 1 ]; then
        echo "SKIPPED: network install test (DFM_SKIP_NETWORK_TESTS=1)"
    else
        step_network_tests
    fi
}

# The install tests need crates.io, so they are #[ignore]d and run explicitly here. Each one
# builds the manager in release mode through bootstrap.sh, so hosted CI runs one nextest
# partition of them per job (DFM_TEST_ARCHIVE and DFM_TEST_PARTITION, as in rust-tests). At most
# four run at once (DFM_NETWORK_THREADS): with twelve at once on an 18-core machine, five failed
# the toolchain digest check and a rerun died (2026-10-20); the cause was not isolated.
step_network_tests() {
    local threads=${DFM_NETWORK_THREADS:-4}
    if [ -n "${DFM_TEST_ARCHIVE:-}" ]; then
        run_archive --run-ignored ignored-only --test-threads "$threads"
    else
        cargo test "${test_select[@]}" -- --ignored --test-threads "$threads"
    fi
}

steps=("$@")
[ ${#steps[@]} -gt 0 ] || steps=(fixtures scripts rust-style rust-tests)
for step in "${steps[@]}"; do
    echo "==> check: $step"
    "step_${step//-/_}"
done
