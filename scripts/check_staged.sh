#!/usr/bin/env bash
set -euo pipefail

git diff --cached --check
gate_tree=$(mktemp -d "${TMPDIR:-/tmp}/dotfiles-manager-gate.XXXXXX")
trap 'rm -rf "$gate_tree"' EXIT

# Check only the proposed commit; unstaged files cannot change its result.
git checkout-index --all --prefix="$gate_tree/"
cd "$gate_tree"
# Hooks export GIT_DIR/GIT_INDEX_FILE; drop them so nested git commands cannot touch this repo.
local_env_vars=$(git rev-parse --local-env-vars)
unset $local_env_vars
bash scripts/check_all.sh
