#!/usr/bin/env bash
# The single shared gate: pre-commit hook, `just check` and hosted CI all run this.
# Usage: check_all.sh [step ...]
#   steps: fixtures scripts rust-style rust-tests network-tests (default: the first four;
#   rust-tests runs network-tests itself unless DFM_SKIP_NETWORK_TESTS=1)
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

# Implementation tests: unit, CLI, bootstrap script and build-controls differential tests.
# The isolated build (feature test-isolation) is needed by the CLI and differential tests.
# DFM_TEST_SELECT holds the cargo package, target and feature selection (default: the whole
# workspace in the isolated build); hosted CI uses it to run one shard of test binaries per job.
# Set DFM_SKIP_NETWORK_TESTS=1 (exactly 1) to skip the network-tests step; the skip is printed.
step_rust_tests() {
    local select
    read -ra select <<<"${DFM_TEST_SELECT:---workspace --features dfm-cli/test-isolation}"
    cargo test --locked "${select[@]}"
    if [ "${DFM_SKIP_NETWORK_TESTS:-}" = 1 ]; then
        echo "SKIPPED: network install test (DFM_SKIP_NETWORK_TESTS=1)"
    else
        step_network_tests
    fi
}

# The install tests need crates.io, so they are #[ignore]d and run explicitly here. Each one
# builds the manager in release mode, so hosted CI shards them: DFM_NETWORK_TESTS holds the
# exact names of one shard (default: every ignored test in the workspace).
step_network_tests() {
    if [ -n "${DFM_NETWORK_TESTS:-}" ]; then
        local names
        read -ra names <<<"$DFM_NETWORK_TESTS"
        cargo test -p dfm-cli --locked --features test-isolation --test bootstrap -- --ignored --exact "${names[@]}"
    else
        cargo test --workspace --locked --features dfm-cli/test-isolation -- --ignored
    fi
}

steps=("$@")
[ ${#steps[@]} -gt 0 ] || steps=(fixtures scripts rust-style rust-tests)
for step in "${steps[@]}"; do
    echo "==> check: $step"
    "step_${step//-/_}"
done
