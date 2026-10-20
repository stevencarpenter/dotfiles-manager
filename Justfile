# Documentation fixtures only. No resolver expectations run and no live configuration operations.
check-fixtures:
    bash scripts/check_all.sh fixtures

# Python helper style, shell syntax and the staged-gate script test.
check-scripts:
    bash scripts/check_all.sh scripts

# cargo fmt --check and cargo clippy -D warnings.
check-style:
    bash scripts/check_all.sh rust-style

# Implementation tests for the Rust workspace and the bootstrap script, network install tests included.
check-tests:
    bash scripts/check_all.sh rust-tests

# The network install tests alone (crates.io access).
check-network:
    bash scripts/check_all.sh network-tests

# The steps the commit hook runs, on the working tree instead of the staged copy.
check-commit:
    bash scripts/check_all.sh fixtures scripts rust-style

# Full local validation. The commit hook runs `check-commit`; hosted CI runs each step in its own job.
check:
    bash scripts/check_all.sh

# Same as `check`.
check-all: check
