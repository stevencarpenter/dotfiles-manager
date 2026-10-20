#!/usr/bin/env bash
# The pre-push gate: runs scripts/check_all.sh rust-tests on every commit being pushed.
# Usage: git pre-push feeds "<local ref> <local sha> <remote ref> <remote sha>" lines on stdin;
# with no stdin lines, HEAD stands in for the pushed tip. Every commit reachable from a pushed
# tip and from no remote-tracking ref is exported with `git archive` and tested in its own
# temporary tree, so unstaged or uncommitted files cannot change the result.
set -euo pipefail

tips=()
lines=0
if [ ! -t 0 ]; then
    while read -r _local_ref local_sha _remote_ref _remote_sha; do
        lines=$((lines + 1))
        # A branch deletion pushes the all-zero sha and has nothing to test.
        case $local_sha in '' | 0000000000000000000000000000000000000000) continue ;; esac
        tips+=("$local_sha")
    done
fi
if [ ${#tips[@]} -eq 0 ]; then
    if [ "$lines" -gt 0 ]; then
        echo "==> pre-push: only deletions; nothing to test"
        exit 0
    fi
    tips=(HEAD)
fi
# Git gives one line per updated ref, not one per commit, and the remote sha of a new branch is
# all zeros. The remote-tracking refs bound the range instead: a commit no remote has is one
# this push introduces; a commit a remote has went through this hook when it was pushed.
# Oldest first, so a broken commit is reported before the commit that fixes it.
# shellcheck disable=SC2207 # object ids hold no whitespace
shas=($(git rev-list --reverse "${tips[@]}" --not --remotes))
if [ ${#shas[@]} -eq 0 ]; then
    echo "==> pre-push: every commit is on a remote already; nothing to test"
    exit 0
fi

# Same build directory as the commit hook: the dependencies are built once per clone.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$(git rev-parse --show-toplevel)/target/gate}"
gate_root=$(mktemp -d "${TMPDIR:-/tmp}/dotfiles-manager-push.XXXXXX")
trap 'rm -rf "$gate_root"' EXIT

for sha in "${shas[@]}"; do
    echo "==> pre-push: testing $sha"
    # A new tree per commit: clearing a shared tree with `rm -rf dir/*` would keep a dotfile
    # of the previous commit (a .cargo/config.toml, for example) in this one's tree.
    gate_tree=$gate_root/$sha
    mkdir "$gate_tree"
    git archive "$sha" | tar -x -C "$gate_tree"
    (
        cd "$gate_tree"
        local_env_vars=$(git rev-parse --local-env-vars)
        # shellcheck disable=SC2086 # the variable holds names to unset
        unset $local_env_vars
        bash scripts/check_all.sh rust-tests
    )
done
