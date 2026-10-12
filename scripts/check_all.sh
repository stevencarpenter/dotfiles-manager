#!/usr/bin/env bash
# The single shared gate: pre-commit hook, `just check` and hosted CI all run this.
# Usage: check_all.sh [step ...]   steps: fixtures scripts rust-style rust-tests (default: all)
set -euo pipefail

# Documentation fixtures only: shape and references. The resolver cases run in step rust-tests.
step_fixtures() {
    uv run scripts/check_examples.py
}

# Python helper style, shell syntax and the staged-gate script test.
step_scripts() {
    uvx --python 3.14 ruff@0.14.14 check scripts
    uvx --python 3.14 ruff@0.14.14 format --check scripts
    # `bash -n a b c` parses only the first file, so each file gets its own run.
    for f in scripts/check_all.sh scripts/check_staged.sh examples/external/modules/work/files/session-hook.sh; do
        bash -n "$f"
    done
    # macOS /bin/sh is bash 3.2 in POSIX mode: `sh -n` checks syntax only, nothing else.
    sh -n scripts/bootstrap.sh
    if command -v shellcheck >/dev/null 2>&1; then
        shellcheck -s sh -S warning scripts/bootstrap.sh
    else
        echo "SKIPPED: shellcheck is not installed"
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
# The install test needs crates.io, so it is #[ignore]d and run explicitly here.
# Set DFM_SKIP_NETWORK_TESTS=1 (exactly 1) to skip it; the skip is printed.
step_rust_tests() {
    cargo test --workspace --locked --features dfm-cli/test-isolation
    if [ "${DFM_SKIP_NETWORK_TESTS:-}" = 1 ]; then
        echo "SKIPPED: network install test (DFM_SKIP_NETWORK_TESTS=1)"
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
