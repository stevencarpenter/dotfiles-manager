#!/usr/bin/env bash
set -euo pipefail

git diff --cached --check
gate_tree=$(mktemp -d "${TMPDIR:-/tmp}/dotfiles-manager-gate.XXXXXX")
trap 'rm -rf "$gate_tree"' EXIT

# Check only the proposed commit; unstaged files cannot change its result.
git checkout-index --all --prefix="$gate_tree/"
cd "$gate_tree"
bash scripts/check_all.sh
