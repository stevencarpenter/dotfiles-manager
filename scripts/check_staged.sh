#!/usr/bin/env bash
# Usage: check_staged.sh [step ...]   runs scripts/check_all.sh with those steps on the staged tree.
set -euo pipefail

git diff --cached --check
# Reuse this clone's dependency builds; the copied tree rebuilds only the workspace crates.
# Not target/ itself: cargo locks a build directory, and a hook must not wait for a running build.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$(git rev-parse --show-toplevel)/target/gate}"
gate_tree=$(mktemp -d "${TMPDIR:-/tmp}/dotfiles-manager-gate.XXXXXX")
trap 'rm -rf "$gate_tree"' EXIT

# Check only the proposed commit; unstaged files cannot change its result.
git checkout-index --all --prefix="$gate_tree/"
cd "$gate_tree"
# Hooks export GIT_DIR/GIT_INDEX_FILE; drop them so nested git commands cannot touch this repo.
local_env_vars=$(git rev-parse --local-env-vars)
unset $local_env_vars
bash scripts/check_all.sh "$@"
