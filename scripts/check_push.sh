#!/usr/bin/env bash
# The pre-push gate: runs scripts/check_all.sh rust-tests on each pushed tip.
# Usage: check_push.sh [remote]. git pre-push names the remote in $1 and feeds "<local ref>
# <local sha> <remote ref> <remote sha>" lines on stdin; with no stdin lines, HEAD stands in
# for the pushed tip. Each pushed tip the remote does not have yet is written out with
# `git checkout-index` and tested in its own temporary tree, so unstaged or uncommitted files
# cannot change the result. Commits below a tip are not tested: the tip supersedes them
# (owner rule, 2026-10-25).
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
# The remote's tracking refs also mark what it has: a tip the remote has went through this
# hook when it was pushed there. A push to a URL has no tracking refs; every remote's refs
# mark it then.
if [ -n "$remote" ] && git remote get-url "$remote" >/dev/null 2>&1; then
    known+=(--remotes="$remote")
else
    known+=(--remotes)
fi
shas=()
for tip in "${tips[@]}"; do
    sha=$(git rev-parse --verify "$tip^{commit}")
    case " ${shas[*]-} " in *" $sha "*) continue ;; esac
    # A tip that the remote's refs reach is on the remote already.
    if [ -n "$(git rev-list -n 1 "$sha" --not "${known[@]}")" ]; then
        shas+=("$sha")
    fi
done
if [ ${#shas[@]} -eq 0 ]; then
    echo "==> pre-push: the remote has every pushed tip; nothing to test"
    exit 0
fi

# Same build directory as the commit hook: the dependencies are built once per clone.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$(git rev-parse --show-toplevel)/target/gate}"
gate_root=$(mktemp -d "${TMPDIR:-/tmp}/dotfiles-manager-push.XXXXXX")
trap 'rm -rf "$gate_root"' EXIT

for sha in "${shas[@]}"; do
    echo "==> pre-push: testing $sha"
    # A new tree per tip: clearing a shared tree with `rm -rf dir/*` would keep a dotfile of
    # the previous tip (a .cargo/config.toml, for example) in this one's tree.
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
