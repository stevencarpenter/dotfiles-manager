#!/usr/bin/env bash
# The pre-push gate: runs scripts/check_all.sh rust-tests on every commit being pushed.
# Usage: check_push.sh [remote]. git pre-push names the remote in $1 and feeds "<local ref>
# <local sha> <remote ref> <remote sha>" lines on stdin; with no stdin lines, HEAD stands in
# for the pushed tip. Every commit reachable from a pushed tip and not yet on the remote is
# written out with `git checkout-index` and tested in its own temporary tree, so unstaged or
# uncommitted files cannot change the result.
set -euo pipefail

remote=${1:-}
tips=()
known=()
lines=0
if [ ! -t 0 ]; then
    while read -r _local_ref local_sha _remote_ref remote_sha; do
        lines=$((lines + 1))
        # A branch deletion pushes the all-zero sha and has nothing to test.
        case $local_sha in '' | 0000000000000000000000000000000000000000) continue ;; esac
        tips+=("$local_sha")
        # The remote's current tip of this ref bounds the update when the object is known
        # locally; a new branch reports all zeros, and an unfetched tip is unknown.
        if [ -n "$remote_sha" ] && git cat-file -e "$remote_sha" 2>/dev/null; then
            known+=("$remote_sha")
        fi
    done
fi
if [ ${#tips[@]} -eq 0 ]; then
    if [ "$lines" -gt 0 ]; then
        echo "==> pre-push: only deletions; nothing to test"
        exit 0
    fi
    tips=(HEAD)
fi
# Git gives one line per updated ref, not one per commit, so the remote's tracking refs bound
# the range as well: a commit the remote has went through this hook when it was pushed there.
# A push to a URL has no tracking refs; every remote's refs bound it then. Oldest first, so a
# broken commit is reported before the commit that fixes it.
if [ -n "$remote" ] && git remote get-url "$remote" >/dev/null 2>&1; then
    known+=(--remotes="$remote")
else
    known+=(--remotes)
fi
# shellcheck disable=SC2207 # object ids hold no whitespace
shas=($(git rev-list --reverse "${tips[@]}" --not "${known[@]}"))
if [ ${#shas[@]} -eq 0 ]; then
    echo "==> pre-push: every commit is on the remote already; nothing to test"
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
    # The commit's own tree through a scratch index, not `git archive`: an archive drops the
    # paths a committed export-ignore attribute names, and their tests would not run.
    GIT_INDEX_FILE=$gate_root/index git read-tree "$sha"
    GIT_INDEX_FILE=$gate_root/index git checkout-index -a --prefix="$gate_tree/"
    (
        cd "$gate_tree"
        local_env_vars=$(git rev-parse --local-env-vars)
        # shellcheck disable=SC2086 # the variable holds names to unset
        unset $local_env_vars
        bash scripts/check_all.sh rust-tests
    )
done
