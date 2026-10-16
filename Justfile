# Documentation fixtures only. No resolver expectations run and no live configuration operations.
check-fixtures:
    bash scripts/check_all.sh fixtures

# Python helper style, shell syntax and the staged-gate script test.
check-scripts:
    bash scripts/check_all.sh scripts

# cargo fmt --check and cargo clippy -D warnings.
check-style:
    bash scripts/check_all.sh rust-style

# Implementation tests for the Rust workspace and the bootstrap script.
check-tests:
    bash scripts/check_all.sh rust-tests

# Full local validation, the same script as the commit hook. Hosted CI runs scripts/check_all.sh once per step, not this recipe.
check:
    bash scripts/check_all.sh

# Same as `check`.
check-all: check
