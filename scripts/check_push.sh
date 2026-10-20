#!/usr/bin/env bash
# The pre-push gate: runs scripts/check_all.sh rust-tests on every commit being pushed.
# Usage: git pre-push feeds "<local ref> <local sha> <remote ref> <remote sha>" lines on stdin;
# with no stdin lines, HEAD is checked. Each distinct commit is exported with `git archive`
# and tested in a temporary tree, so unstaged or uncommitted files cannot change the result.
set -euo pipefail

shas=()
lines=0
if [ ! -t 0 ]; then
    while read -r _local_ref local_sha _remote_ref _remote_sha; do
        lines=$((lines + 1))
        # A branch deletion pushes the all-zero sha and has nothing to test.
        case $local_sha in '' | 0000000000000000000000000000000000000000) continue ;; esac
        shas+=("$local_sha")
    done
fi
if [ ${#shas[@]} -eq 0 ]; then
    if [ "$lines" -gt 0 ]; then
        echo "==> pre-push: only deletions; nothing to test"
        exit 0
    fi
    shas=("$(git rev-parse HEAD)")
fi

# Same build directory as the commit hook: the dependencies are built once per clone.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$(git rev-parse --show-toplevel)/target/gate}"
gate_tree=$(mktemp -d "${TMPDIR:-/tmp}/dotfiles-manager-push.XXXXXX")
trap 'rm -rf "$gate_tree"' EXIT

seen=" "
for sha in "${shas[@]}"; do
    case $seen in *" $sha "*) continue ;; esac
    seen="$seen$sha "
    echo "==> pre-push: testing $sha"
    rm -rf "${gate_tree:?}"/*
    git archive "$sha" | tar -x -C "$gate_tree"
    (
        cd "$gate_tree"
        local_env_vars=$(git rev-parse --local-env-vars)
        # shellcheck disable=SC2086 # the variable holds names to unset
        unset $local_env_vars
        bash scripts/check_all.sh rust-tests
    )
done
