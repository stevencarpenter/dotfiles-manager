#!/bin/sh
# Bootstrap the dotfiles manager from a verified source checkout.
# Contract: docs/architecture.md section 12. Procedure and limits: docs/bootstrap.md.
#
#   bootstrap.sh install --source <git checkout> --commit <40 hex>
#   bootstrap.sh check-controls --source <staged tree>   (read-only)
#   bootstrap.sh uninstall
#   bootstrap.sh toolchain-digest --dir <installed toolchain>   (value for the lock)
#
# POSIX sh plus system tools from a fixed PATH. No cargo, rustc or rustup process
# starts before the toolchain tree digest and the build controls pass.
# Exit: 0 ok, 1 build failed, 2 usage, 3 refused before the build.
# Scrub the caller's environment before anything else runs: exported shell functions
# (BASH_FUNC_*), PERL5LIB or PERL5OPT (shasum is perl), CDPATH, ENV and the like must not
# influence the checks. Only the test hooks below survive, and only when set.
# Run as `/usr/bin/env -i /bin/sh scripts/bootstrap.sh ...`: a SHELLOPTS=noexec in the
# caller's environment makes /bin/sh read this file without running it, so treat a run
# that did not print its final line as a failure.
if [ -z "${DFM_BS_CLEAN:-}" ]; then
  exec /usr/bin/env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin HOME="${HOME:-}" DFM_BS_CLEAN=1 \
    ${DFM_BOOTSTRAP_TEST:+"DFM_BOOTSTRAP_TEST=$DFM_BOOTSTRAP_TEST"} ${DFM_TEST_HOME:+"DFM_TEST_HOME=$DFM_TEST_HOME"} \
    ${DFM_TEST_OS:+"DFM_TEST_OS=$DFM_TEST_OS"} ${DFM_TEST_ARCH:+"DFM_TEST_ARCH=$DFM_TEST_ARCH"} \
    ${DFM_TEST_CLT:+"DFM_TEST_CLT=$DFM_TEST_CLT"} /bin/sh "$0" "$@"
fi
set -eu
umask 077
unset CDPATH
SYSPATH=/usr/bin:/bin:/usr/sbin:/sbin # same list as dfm_platform_macos::SYSTEM_PATH
PATH=$SYSPATH
export PATH LC_ALL=C
DATA_REL=.local/share/dotfiles-manager # same literal as dfm_platform_macos::DATA_DIR
CACHE_REL=.cache/dotfiles-manager      # same literal as dfm_platform_macos::CACHE_DIR
TAB=$(printf '\t')
NL='
'

refuse() { printf 'error: %s %s\n' "$1" "$2" >&2; exit 3; }
usage() { printf 'error: E_USAGE %s\n' "$1" >&2; exit 2; }
# $1 is awk output that starts with an error code.
refuse_out() { [ -z "$1" ] || printf 'error: %s\n' "$1" >&2; exit 3; }

# Test hooks exist only when DFM_BOOTSTRAP_TEST=1 and replace the dscl and uname answers.
test_mode() { [ "${DFM_BOOTSTRAP_TEST:-}" = 1 ]; }

resolve_home() {
  if test_mode && [ -n "${DFM_TEST_HOME:-}" ]; then
    pw_home=$DFM_TEST_HOME
  else
    user=$(/usr/bin/id -un)
    pw_home=$(/usr/bin/dscl . -read "/Users/$user" NFSHomeDirectory 2>/dev/null | /usr/bin/sed -n 's/^NFSHomeDirectory: //p') || pw_home=
  fi
  [ -n "$pw_home" ] || refuse E_HOME_MISMATCH "cannot read the home directory from the password database"
  [ "$pw_home" = "${HOME:-}" ] || refuse E_HOME_MISMATCH "password database home $pw_home differs from HOME"
  HOME_DIR=$pw_home
  DATA=$HOME_DIR/$DATA_REL
  CACHE=$HOME_DIR/$CACHE_REL
}

check_platform() {
  os=$(/usr/bin/uname -s); arch=$(/usr/bin/uname -m)
  if test_mode; then os=${DFM_TEST_OS:-$os}; arch=${DFM_TEST_ARCH:-$arch}; fi
  [ "$os" = Darwin ] && [ "$arch" = arm64 ] ||
    refuse E_PLATFORM_UNSUPPORTED "$os/$arch is not supported; native Apple Silicon macOS is required"
}

# ---- restricted-grammar checker (mirror of dfm_model::controls) ----------------------------
# The scanner accepts: comments, [table] and [[table]] headers with bare dotted names, and
# bare `key = value` lines where value is a single-line string without escapes, boolean,
# integer, array of those, or inline table of those. Anything else is refused.
SCAN='
function bad(m) { print "E_SOURCE_UNTRUSTED " FN ": line " NR ": " m; failed = 1; exit 3 }
function ws() { while (substr(S, P, 1) == " " || substr(S, P, 1) == "\t") P++ }
function isb(c) { return c ~ /^[A-Za-z0-9_-]$/ }
function scalar(   c, e, r) {
  c = substr(S, P, 1)
  if (c == "\"" || c == "\047") {
    e = index(substr(S, P + 1), c)
    if (e == 0) bad("unterminated string")
    VS = substr(S, P + 1, e - 1)
    if (c == "\"" && index(VS, "\\") > 0) bad("escape in string")
    P += e + 1; VK = "s"; return
  }
  r = substr(S, P)
  if (match(r, /^(true|false)/)) {
    P += RLENGTH; VK = "b"; VS = ""
    if (isb(substr(S, P, 1))) bad("unrecognised value")
    return
  }
  if (match(r, /^[+-]?[0-9][0-9_]*/)) {
    P += RLENGTH; VK = "i"; VS = ""
    if (substr(S, P, 1) ~ /^[A-Za-z0-9_.:-]$/) bad("unrecognised number")
    return
  }
  bad("unrecognised value")
}
function array(   c) {
  P++; ARRN = 0; delete ARR; ws()
  if (substr(S, P, 1) == "]") { P++; VK = "a"; return }
  while (1) {
    ws(); scalar(); ARRN++; ARR[ARRN] = (VK == "s") ? VS : "\001"
    ws(); c = substr(S, P, 1)
    if (c == ",") { P++; ws(); if (substr(S, P, 1) == "]") { P++; break }; continue }
    if (c == "]") { P++; break }
    bad("array syntax")
  }
  VK = "a"
}
function inline(key,   sk, c) {
  P++; ws(); delete SUBS
  if (substr(S, P, 1) == "}") { P++; VK = "t"; return }
  while (1) {
    ws()
    if (!match(substr(S, P), /^[A-Za-z0-9_-]+/)) bad("inline table key")
    sk = substr(S, P, RLENGTH); P += RLENGTH; ws()
    if (sk in SUBS) bad("duplicate inline table key")
    SUBS[sk] = 1
    if (substr(S, P, 1) != "=") bad("inline table syntax")
    P++; ws()
    if (substr(S, P, 1) == "[") array(); else scalar()
    on_inline(key, sk)
    ws(); c = substr(S, P, 1)
    if (c == ",") { P++; continue }
    if (c == "}") { P++; break }
    bad("inline table syntax")
  }
  VK = "t"
}
function header(   arr, name, cl) {
  arr = (substr(S, 1, 2) == "[[")
  P = arr ? 3 : 2
  if (!match(substr(S, P), /^[A-Za-z0-9_-]+(\.[A-Za-z0-9_-]+)*/)) bad("table header")
  name = substr(S, P, RLENGTH); P += RLENGTH
  cl = arr ? "]]" : "]"
  if (substr(S, P, length(cl)) != cl) bad("table header")
  P += length(cl); ws()
  if (P <= length(S) && substr(S, P, 1) != "#") bad("text after header")
  if (!arr) { if (name in TABLES) bad("duplicate table"); TABLES[name] = 1 }
  delete KEYS
  on_header(name, arr)
}
function pair(   key, c) {
  if (!match(S, /^[A-Za-z0-9_-]+/)) bad("key")
  key = substr(S, 1, RLENGTH); P = RLENGTH + 1; ws()
  if (key in KEYS) bad("duplicate key")
  KEYS[key] = 1
  if (substr(S, P, 1) != "=") bad("expected =")
  P++; ws(); c = substr(S, P, 1)
  if (c == "[") array(); else if (c == "{") inline(key); else scalar()
  ws()
  if (P <= length(S) && substr(S, P, 1) != "#") bad("text after value")
  on_pair(key)
}
{
  S = $0; sub(/^[ \t]+/, "", S); sub(/[ \t]+$/, "", S)
  if (S == "" || substr(S, 1, 1) == "#") next
  if (substr(S, 1, 1) == "[") header(); else pair()
}
'

MANIFEST_RULES='
function pathok(v, d,   n, i, c, comp, seen) {
  if (v == "" || substr(v, 1, 1) == "/" || substr(v, 1, 1) == "~" || index(v, "\\") > 0) return 0
  n = split(v, comp, "/"); seen = 0
  for (i = 1; i <= n; i++) {
    c = comp[i]
    if (c == "" || c == ".") continue
    if (c == "..") { if (seen) return 0; d--; if (d < 0) return 0 } else seen = 1
  }
  return 1
}
function classify(t,   n, s, i, k, rest) {
  n = split(t, s, ".")
  if (s[1] == "replace") bad("[replace] is not allowed")
  if (s[1] == "patch") {
    if (n < 2 || s[2] != "crates-io") bad("[patch] source is not crates-io")
    rest = n - 2
  } else {
    k = 0
    for (i = 1; i <= n; i++) if (s[i] == "dependencies" || s[i] == "dev-dependencies" || s[i] == "build-dependencies") { k = i; break }
    if (k == 0) return "ord"
    rest = n - k
  }
  if (rest == 0) return "dep"
  if (rest == 1) return "depsub"
  bad("nested dependency table")
}
function on_header(name, arr) {
  CTX = classify(name); TABLE = name
  if (arr && CTX != "ord") bad("array of dependency tables")
}
function on_pair(key,   i) {
  if (key == "path" && (VK != "s" || !pathok(VS, DEPTH))) bad("path escapes the staged tree")
  if (CTX == "dep") {
    if (VK != "s" && VK != "t") bad("unsupported dependency form")
  } else if (CTX == "depsub") {
    if (!(key in ALLOW)) bad("unsupported dependency key " key)
    if (VK == "t") bad("inline table in dependency table")
  } else {
    if (VK == "t" && (key in DENY)) bad("table-valued key " key)
    if (VK == "s" && (key == "build" || key == "workspace") && !pathok(VS, DEPTH)) bad(key " escapes the staged tree")
    if (TABLE == "workspace" && (key == "members" || key == "exclude" || key == "default-members")) {
      if (VK != "a") bad(key " is not an array")
      for (i = 1; i <= ARRN; i++) if (ARR[i] == "\001" || !pathok(ARR[i], DEPTH)) bad(key " entry escapes the staged tree")
    }
  }
}
function on_inline(key, sk) {
  if (sk == "path" && (VK != "s" || !pathok(VS, DEPTH))) bad("path escapes the staged tree")
  if (CTX == "dep" && !(sk in ALLOW)) bad("unsupported dependency key " sk)
}
BEGIN {
  CTX = "ord"; TABLE = ""
  split("version features optional default-features default_features package workspace path artifact lib target", a, " ")
  for (i in a) ALLOW[a[i]] = 1
  split("patch replace dependencies dev-dependencies build-dependencies target workspace", a, " ")
  for (i in a) DENY[a[i]] = 1
}
END { if (failed) exit 3 }
'

LOCK_RULES='
function on_header(name, arr) {
  if (name == "manager") { if (MSEEN) bad("duplicate [manager]"); MSEEN = 1 }
  TABLE = name
}
function on_inline(k, sk) { bad("inline table in lock") }
function on_pair(key) {
  if (TABLE != "manager") return
  if (key == "toolchain") {
    if (PIN != "" || VK != "s" || VS !~ /^[0-9]+\.[0-9]+\.[0-9]+$/) bad("invalid or repeated toolchain")
    PIN = VS
  } else if (key == "toolchain_tree_sha256") {
    if (TREE != "" || VK != "s" || length(VS) != 64 || VS !~ /^[0-9a-f]+$/) bad("invalid or repeated toolchain_tree_sha256")
    TREE = VS
  }
}
END {
  if (failed) exit 3
  if (PIN == "" || TREE == "") { FN = FILENAME; NR = 0; bad("[manager] needs toolchain and toolchain_tree_sha256") }
  print "PIN " PIN; print "TREE " TREE
}
'

TOOLCHAIN_RULES='
function on_header(name, arr) {
  if (name != "toolchain" || arr || TABLE != "") bad("unsupported table")
  TABLE = name
}
function on_inline(k, sk) { bad("inline table") }
function on_pair(key,   i) {
  if (TABLE == "toolchain" && key == "components" && VK == "a") {
    for (i = 1; i <= ARRN; i++) if (ARR[i] != "rustfmt" && ARR[i] != "clippy") bad("rust-toolchain.toml differs from the lock pin")
    return
  }
  if (TABLE != "toolchain" || key != "channel" || VK != "s" || VS != PINV || CSEEN) bad("rust-toolchain.toml differs from the lock pin")
  CSEEN = 1
}
END { if (failed) exit 3; if (!CSEEN) { NR = 0; bad("no channel") } }
'

CARGO_LOCK_AWK='
function bad(m) { print "E_SOURCE_UNTRUSTED Cargo.lock: line " NR ": " m; exit 3 }
{
  t = $0; sub(/^[ \t]+/, "", t); sub(/[ \t]+$/, "", t)
  if (t == "" || substr(t, 1, 1) == "#") next
  if (t == "[[package]]" || t == "]" || t == "dependencies = [") next
  if (t ~ /^version = [0-9]+$/ || t ~ /^version = "[0-9A-Za-z.+-]+"$/) next
  if (t ~ /^name = "[A-Za-z0-9_-]+"$/ || t ~ /^checksum = "[0-9a-f]+"$/) next
  if (t == "source = \"registry+https://github.com/rust-lang/crates.io-index\"" || t == "source = \"sparse+https://index.crates.io/\"") next
  if (t ~ /^"[-A-Za-z0-9_.+ ():\/]+",?$/) next
  bad("line not recognised or not registry-only")
}
'

# Runs an awk program over a staged file after the size and character checks.
# $1 stage, $2 relative path, $3 program, rest: extra awk -v assignments.
run_awk() {
  _stage=$1 _rel=$2 _prog=$3; shift 3
  _file=$_stage/$_rel
  [ "$(wc -c < "$_file" | tr -d ' ')" -le 1048576 ] || refuse E_SOURCE_UNTRUSTED "$_rel exceeds 1 MiB"
  [ "$(tr -d ' -~\t\n' < "$_file" | wc -c | tr -d ' ')" -eq 0 ] || refuse E_SOURCE_UNTRUSTED "$_rel: non-ASCII or control character"
  awk -v "FN=$_rel" "$@" "$_prog" "$_file" || return 1
}

controls_staged() { # $1 stage; sets PIN and TREE
  stage=$1
  [ -d "$stage" ] && [ ! -L "$stage" ] || refuse E_SOURCE_UNTRUSTED "staged tree is not a directory"
  [ -z "$(cd "$stage" && find . 2>&1 >/dev/null)" ] || refuse E_SOURCE_UNTRUSTED "unreadable entry in staged tree"
  odd=$(cd "$stage" && find . ! -name . ! -type f ! -type d -print | head -n 1)
  [ -z "$odd" ] || refuse E_SOURCE_UNTRUSTED "symlink or special file in staged tree: $odd"
  bad=$(cd "$stage" && { find . -name "*$NL*" -print; find . -print | grep '[^ -~]' || true; } | head -n 1)
  [ -z "$bad" ] || refuse E_SOURCE_UNTRUSTED "unsupported file name in staged tree"
  [ -f "$stage/dotfiles-manager.lock" ] || refuse E_SOURCE_UNTRUSTED "dotfiles-manager.lock missing"
  out=$(run_awk "$stage" dotfiles-manager.lock "$SCAN$LOCK_RULES") || refuse_out "$out"
  PIN=$(printf '%s\n' "$out" | sed -n 's/^PIN //p'); TREE=$(printf '%s\n' "$out" | sed -n 's/^TREE //p')
  [ -f "$stage/Cargo.lock" ] || refuse E_SOURCE_UNTRUSTED "Cargo.lock missing"
  out=$(run_awk "$stage" Cargo.lock "$CARGO_LOCK_AWK") || refuse_out "$out"
  list=$(cd "$stage" && find . -type f | sort)
  # Names are matched case-insensitively because the file system is.
  clash=$(printf '%s\n' "$list" | awk '{ sub(/^\.\//, ""); n = split($0, c, "/"); p = ""
    for (i = 1; i <= n; i++) { p = (i > 1 ? p "/" : "") c[i]; k = tolower(p)
      if (!(k in S)) S[k] = p; else if (S[k] != p) { print p; exit } } }')
  [ -z "$clash" ] || refuse E_SOURCE_UNTRUSTED "$clash: differs from another path only in case"
  root_ws=0
  while IFS= read -r f; do
    f=${f#./}
    lf=$(printf '%s' "$f" | tr 'A-Z' 'a-z')
    case $lf in
      .cargo/config.toml|.cargo/config|*/.cargo/config.toml|*/.cargo/config)
        refuse E_SOURCE_UNTRUSTED "$f: cargo configuration in staged tree" ;;
    esac
    case ${lf##*/} in
      rust-toolchain) refuse E_SOURCE_UNTRUSTED "$f: legacy rust-toolchain file" ;;
      rust-toolchain.toml)
        out=$(run_awk "$stage" "$f" "$SCAN$TOOLCHAIN_RULES" -v "PINV=$PIN") || refuse_out "$out" ;;
      cargo.toml)
        depth=$(printf '%s' "$f" | tr -cd / | wc -c | tr -d ' ')
        out=$(run_awk "$stage" "$f" "$SCAN$MANIFEST_RULES" -v "DEPTH=$depth") || refuse_out "$out"
        if [ "$depth" -eq 0 ] && grep -Eq "^[ $TAB]*\\[workspace\\][ $TAB]*(#.*)?\$" "$stage/$f"; then root_ws=1; fi ;;
    esac
  done <<EOF
$list
EOF
  [ "$root_ws" -eq 1 ] || refuse E_SOURCE_UNTRUSTED "the root Cargo.toml must declare [workspace]"
}

# Prints the toolchain tree digest of $1 (docs/architecture.md section 8, item 4a).
# Fails closed: any unreadable entry, failed hash, bad symlink or empty tree is an error,
# never a digest of the entries that happened to be readable.
toolchain_digest() (
  cd "$1" || exit 1
  [ -z "$(find . 2>&1 >/dev/null)" ] || exit 1
  [ -z "$(find . ! -name . ! -type f ! -type d ! -type l -print | head -n 1)" ] || exit 1
  [ -z "$(find . -name "*$NL*" -print | head -n 1)" ] || exit 1
  ! find . -print | grep -q '[^ -~]' || exit 1
  nf=$(find . -type f | wc -l); nl=$(find . -type l | wc -l)
  [ "$nf" -gt 0 ] || exit 1
  plain=$(find . -type f ! -perm -u+x -print0 | xargs -0 -r /usr/bin/shasum -a 256 |
    awk '{ h = $1; sub(/^[0-9a-f]+  \.\//, ""); printf "%s\tfile\t%s\n", $0, h }')
  execs=$(find . -type f -perm -u+x -print0 | xargs -0 -r /usr/bin/shasum -a 256 |
    awk '{ h = $1; sub(/^[0-9a-f]+  \.\//, ""); printf "%s\texec\t%s\n", $0, h }')
  links=$(find . -type l | while IFS= read -r l; do printf '%s\t%s\n' "${l#./}" "$(readlink "$l")"; done |
    awk -F'\t' '
      NF != 2 || $2 ~ /[^ -~]/ || $2 == "" || substr($2, 1, 1) == "/" || substr($2, 1, 1) == "~" || index($2, "\\") > 0 { exit 1 }
      { d = gsub(/\//, "/", $1); n = split($2, c, "/"); seen = 0
        for (i = 1; i <= n; i++) {
          if (c[i] == "" || c[i] == ".") continue
          if (c[i] == "..") { if (seen) exit 1; d--; if (d < 0) exit 1 } else seen = 1
        }
        printf "%s\tsymlink\t%s\n", $1, $2 }')
  entries=$(printf '%s\n%s\n%s\n' "$plain" "$execs" "$links" | grep .) || exit 1
  [ "$(printf '%s\n' "$entries" | wc -l | tr -d ' ')" -eq $((nf + nl)) ] || exit 1
  printf '%s\n' "$entries" | sort | while IFS=$TAB read -r p c v; do printf '%s\0%s\0%s\n' "$p" "$c" "$v"; done |
    /usr/bin/shasum -a 256 | cut -d' ' -f1
)

controls_toolchain() { # uses PIN, TREE, DATA
  TC=$DATA/toolchains/$PIN
  [ -d "$TC" ] && [ ! -L "$TC" ] ||
    refuse E_TOOLCHAIN_MISMATCH "toolchain $PIN is absent at $TC; install exactly that version there (official Rust release distribution)"
  got=$(toolchain_digest "$TC") || got=
  [ -n "$PIN" ] && [ -n "$TREE" ] && [ -n "$got" ] && [ "$got" = "$TREE" ] || refuse E_TOOLCHAIN_MISMATCH "toolchain $PIN tree digest differs from the lock"
}

controls_ancestors() { # uses CACHE
  d=$CACHE/build; tail=
  while [ ! -d "$d" ]; do tail=/${d##*/}$tail; d=$(dirname "$d"); done
  base=$(cd "$d" 2>/dev/null && pwd -P) || refuse E_BUILD_CONFIG "cannot resolve the build directory $d"
  [ "$base" = / ] && base=
  d=$base$tail; [ -n "$d" ] || d=/
  while :; do
    # An ancestor that cannot be searched cannot be shown to be free of config.
    { [ ! -d "$d" ] || [ -x "$d" ]; } || refuse E_BUILD_CONFIG "cannot search $d to check for Cargo or rustup configuration"
    for f in .cargo/config.toml .cargo/config rust-toolchain rust-toolchain.toml; do
      if [ -e "${d%/}/$f" ] || [ -L "${d%/}/$f" ]; then
        refuse E_BUILD_CONFIG "${d%/}/$f would be read by Cargo or rustup for the build directory"
      fi
    done
    [ "$d" = / ] && break
    d=$(dirname "$d")
  done
}

controls() { controls_staged "$1"; controls_toolchain; controls_ancestors; }

# ---- install and uninstall -----------------------------------------------------------------
ensure_clt() {
  /usr/bin/xcode-select -p >/dev/null 2>&1 && /usr/bin/xcrun --find clang >/dev/null 2>&1 && return 0
  printf 'Command Line Tools are missing; starting the installer\n'
  /usr/bin/xcode-select --install >/dev/null 2>&1 || true
  waited=0
  while [ "$waited" -lt 1800 ]; do
    /usr/bin/xcode-select -p >/dev/null 2>&1 && /usr/bin/xcrun --find clang >/dev/null 2>&1 && return 0
    sleep 10; waited=$((waited + 10))
  done
  refuse E_PREREQ_MISSING "Command Line Tools did not become available within 30 minutes"
}

# Replacement refs (refs/replace) would let a ref swap the content of the verified commit.
git_clean() { /usr/bin/env -i PATH="$SYSPATH" HOME=/var/empty LANG=C GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null \
  GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0 GIT_NO_REPLACE_OBJECTS=1 /usr/bin/git -c core.fsmonitor=false \
  -c core.hooksPath=/dev/null -c core.useReplaceRefs=false "$@"; }

cmd_install() {
  [ -n "${src:-}" ] && [ -n "${commit:-}" ] || usage "install needs --source and --commit"
  # The pin is a full-length lowercase object id and nothing else: no abbreviation, ref, tag,
  # revision syntax (^, ~, @{}), option-like value or whitespace ever reaches git.
  case $commit in ""|*[!0-9a-f]*) refuse E_SOURCE_UNTRUSTED "--commit must be a full lowercase hex object id (40 or 64 digits)" ;; esac
  case ${#commit} in 40) fmt=sha1 ;; 64) fmt=sha256 ;; *) refuse E_SOURCE_UNTRUSTED "--commit must be a full lowercase hex object id (40 or 64 digits)" ;; esac
  resolve_home; check_platform
  [ -d "$src" ] || refuse E_SOURCE_UNTRUSTED "source $src is not a directory"
  # Read-only prerequisite and identity checks come first.
  [ "$(git_clean -C "$src" rev-parse --show-object-format 2>/dev/null)" = "$fmt" ] ||
    refuse E_SOURCE_UNTRUSTED "the source checkout does not use the $fmt object format of the pin"
  resolved=$(git_clean -C "$src" rev-parse --verify --end-of-options "$commit^{commit}" 2>/dev/null) || resolved=
  [ "$resolved" = "$commit" ] || refuse E_SOURCE_UNTRUSTED "$commit is not a commit in the source checkout"
  # Early, read-only hint of the pin so a missing toolchain is refused before any mutation.
  # The real parse and every other check run later on the staged copy.
  hint=$(git_clean -C "$src" cat-file blob "$commit:dotfiles-manager.lock" 2>/dev/null |
    sed -n 's/^toolchain = "\([0-9][0-9.]*\)"[ \t]*$/\1/p' | head -n 1)
  [ -n "$hint" ] || refuse E_SOURCE_UNTRUSTED "dotfiles-manager.lock has no toolchain pin"
  [ -d "$DATA/toolchains/$hint" ] ||
    refuse E_TOOLCHAIN_MISMATCH "toolchain $hint is absent at $DATA/toolchains/$hint; install exactly that version there (official Rust release distribution)"
  ensure_clt
  build=$CACHE/build
  rm -rf "$build"; mkdir -p "$build/stage"
  # Build the verified commit itself, not whatever HEAD is. Both steps are checked: a pipe would hide
  # the status of git archive.
  git_clean -C "$src" archive --format=tar --end-of-options "$commit" >"$build/source.tar" || { echo "error: git archive failed" >&2; exit 1; }
  tar -xf "$build/source.tar" -C "$build/stage" || { echo "error: cannot extract the source" >&2; exit 1; }
  rm -f "$build/source.tar"
  controls "$build/stage"
  # Controls passed. From here the pinned toolchain runs under a constructed environment.
  for d in cargo-home rustup-home build-home; do rm -rf "${DATA:?}/$d"; mkdir -p "$DATA/$d"; done
  cd "$build"
  tcrun() { /usr/bin/env -i PATH="$TC/bin:$SYSPATH" HOME="$DATA/build-home" CARGO_HOME="$DATA/cargo-home" \
    RUSTUP_HOME="$DATA/rustup-home" RUSTC="$TC/bin/rustc" LANG=C DFM_BUILD_COMMIT="$commit" "$@"; }
  tcrun "$TC/bin/cargo" fetch --locked --manifest-path "$build/stage/Cargo.toml" || { echo "error: cargo fetch failed" >&2; exit 1; }
  tcrun "$TC/bin/cargo" build --release --locked --offline --manifest-path "$build/stage/Cargo.toml" \
    --target-dir "$build/target" -p dfm-cli || { echo "error: cargo build failed" >&2; exit 1; }
  bin=$build/target/release/dotfiles-manager
  mkdir -p "$build/fixture"
  tcrun "$bin" self-check --fixture-dir "$build/fixture" >/dev/null || { echo "error: self-check failed" >&2; exit 1; }
  ver=$(tcrun "$bin" --version) || { echo "error: the built binary did not run" >&2; exit 1; }
  case $ver in "dotfiles-manager "*" (commit $commit)") ;; *) echo "error: the built binary reports a different identity: $ver" >&2; exit 1 ;; esac
  semver=${ver#dotfiles-manager }; semver=${semver%% *}
  case $semver in ""|*[!0-9.]*) echo "error: unexpected version $semver" >&2; exit 1 ;; esac
  name=$semver-$(printf '%s' "$commit" | cut -c1-12)
  mkdir -p "$DATA/versions/$name" "$HOME_DIR/.local/bin"
  cp "$bin" "$DATA/versions/$name/dotfiles-manager.new"; chmod 755 "$DATA/versions/$name/dotfiles-manager.new"
  mv -f "$DATA/versions/$name/dotfiles-manager.new" "$DATA/versions/$name/dotfiles-manager"
  ln -s "versions/$name" "$DATA/current.new"; mv -fh "$DATA/current.new" "$DATA/current"
  ln -s "$DATA/current/dotfiles-manager" "$HOME_DIR/.local/bin/dotfiles-manager.new"
  mv -fh "$HOME_DIR/.local/bin/dotfiles-manager.new" "$HOME_DIR/.local/bin/dotfiles-manager"
  rm -rf "$build" "$DATA/cargo-home" "$DATA/rustup-home" "$DATA/build-home"
  sha=$(/usr/bin/shasum -a 256 "$DATA/versions/$name/dotfiles-manager" | cut -d' ' -f1)
  printf 'installed %s\nsource commit %s\nbinary sha256 %s (record only)\nbootstrap: done\n' "$DATA/versions/$name" "$commit" "$sha"
}

# Removes what install created. The toolchain, state and binding are never touched.
cmd_uninstall() {
  resolve_home
  link=$HOME_DIR/.local/bin/dotfiles-manager
  if [ -L "$link" ]; then rm -f "$link"; fi
  rm -rf "${DATA:?}/versions" "$DATA/current" "$DATA/current.new" "$DATA/cargo-home" "$DATA/rustup-home" "$DATA/build-home" "${CACHE:?}"
  rmdir "$DATA" "$HOME_DIR/.local/bin" 2>/dev/null || true
  printf 'removed the installed manager; toolchains, state and binding were left in place\n'
}

cmd=${1:-}; [ $# -gt 0 ] && shift
src= commit= dir=
while [ $# -gt 0 ]; do
  case $1 in
    --source) [ $# -ge 2 ] || usage "--source needs a value"; src=$2; shift 2 ;;
    --commit) [ $# -ge 2 ] || usage "--commit needs a value"; commit=$2; shift 2 ;;
    --dir) [ $# -ge 2 ] || usage "--dir needs a value"; dir=$2; shift 2 ;;
    *) usage "unknown argument $1" ;;
  esac
done
# Relative arguments become absolute so a later cd cannot be redirected.
case $src in ""|/*) ;; *) src=$PWD/$src ;; esac
case $dir in ""|/*) ;; *) dir=$PWD/$dir ;; esac
case $cmd in
  install) cmd_install ;;
  uninstall) cmd_uninstall ;;
  toolchain-digest) # prints the value for [manager].toolchain_tree_sha256 in dotfiles-manager.lock
    [ -d "${dir:-}" ] || usage "toolchain-digest needs --dir <installed toolchain>"
    d=$(toolchain_digest "$dir") && [ -n "$d" ] || refuse E_TOOLCHAIN_MISMATCH "cannot digest $dir"
    printf '%s\n' "$d" ;;
  check-controls)
    [ -n "$src" ] || usage "check-controls needs --source"
    resolve_home; controls "$src"; echo "build controls ok" ;;
  *) usage "usage: bootstrap.sh install --source <git checkout> --commit <sha> | check-controls --source <tree> | uninstall" ;;
esac
