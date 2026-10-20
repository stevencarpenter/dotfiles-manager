#!/bin/sh
# Bootstrap the dotfiles manager from a verified source checkout.
# Contract: docs/architecture.md section 12. Procedure and limits: docs/bootstrap.md.
#
#   bootstrap.sh install [--source <git checkout | https:// or file:// URL>] --commit <40 hex>
#   bootstrap.sh check-controls --source <staged tree>   (read-only)
#   bootstrap.sh uninstall
#   bootstrap.sh toolchain-digest --dir <installed toolchain>   (value for the lock)
#   any command: --expect-sha256 <64 hex>   (this file must have that digest, checked first)
#
# install reads every source through a bare clone (transfer.fsckObjects) in the scratch build
# directory: a local checkout as file://<path>, a URL as given, and without --source the canonical
# origin (ORIGIN below). Network access happens only in that clone.
# install also refuses to run unless this file is byte-identical to scripts/bootstrap.sh of the
# pinned commit, so the script that runs is always the script of the commit it builds.
#
# POSIX sh plus system tools from a fixed PATH. No cargo, rustc or rustup process
# starts before the toolchain tree digest and the build controls pass.
# Exit: 0 ok, 1 failed (build, or a check after the first mutation), 2 usage, 3 refused before any
# mutation. The first mutation is the reset of cargo-home, rustup-home and build-home in the data
# directory. Before it, only the scratch build directory exists, and the exit trap removes it.
# Scrub the caller's environment before anything else runs: exported shell functions of any
# name (BASH_FUNC_*, including ones named like `[` or `test`), PERL5LIB or PERL5OPT (shasum
# is perl), CDPATH, ENV and the like must not influence the checks. Before the scrub this
# stage uses only `case`, `exec`, `shift` and absolute-path commands, so no imported function
# can intercept it. The marker is an argument, not an environment variable, so a caller
# cannot export it to skip the re-execution. A caller can still pass the argument itself,
# so the child checks that its environment holds only the allowed names and refuses
# otherwise. The DFM_TEST_* hooks are forwarded only when DFM_BOOTSTRAP_TEST=1.
# Run it as `/bin/sh scripts/bootstrap.sh ...`, never with bash: in non-POSIX mode an exported
# function can override the exec builtin before the scrub. Not with `env -i`: that clears HOME,
# which must equal the password-database home. Not through `curl | sh`: the script re-executes
# and digests itself by its path ($0), which a pipe does not have. SHELLOPTS and PS4 (xtrace
# with command substitution, noexec) take effect before the first line of this file and are
# outside its control; a run that did not print its final line did not finish.
case ${1:-} in
  --dfm-scrubbed) ;;
  *)
    case ${DFM_BOOTSTRAP_TEST:-} in
      1)
        exec /usr/bin/env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin HOME="${HOME:-}" DFM_BOOTSTRAP_TEST=1 \
          ${DFM_TEST_HOME:+"DFM_TEST_HOME=$DFM_TEST_HOME"} ${DFM_TEST_OS:+"DFM_TEST_OS=$DFM_TEST_OS"} \
          ${DFM_TEST_ARCH:+"DFM_TEST_ARCH=$DFM_TEST_ARCH"} ${DFM_TEST_CLT:+"DFM_TEST_CLT=$DFM_TEST_CLT"} \
          ${DFM_TEST_BEFORE_SWAP:+"DFM_TEST_BEFORE_SWAP=$DFM_TEST_BEFORE_SWAP"} \
          ${DFM_TEST_AFTER_SWAP:+"DFM_TEST_AFTER_SWAP=$DFM_TEST_AFTER_SWAP"} \
          ${DFM_TEST_BEFORE_COPY:+"DFM_TEST_BEFORE_COPY=$DFM_TEST_BEFORE_COPY"} \
          ${DFM_TEST_UID:+"DFM_TEST_UID=$DFM_TEST_UID"} \
          /bin/sh "$0" --dfm-scrubbed "$@" ;;
    esac
    exec /usr/bin/env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin HOME="${HOME:-}" /bin/sh "$0" --dfm-scrubbed "$@" ;;
esac
shift
env_check=$(/usr/bin/env | /usr/bin/awk -F= -v test="${DFM_BOOTSTRAP_TEST:-}" '
  BEGIN { split("PATH HOME PWD OLDPWD SHLVL _", a, " "); for (i in a) ok[a[i]] = 1
    if (test == "1") { split("DFM_BOOTSTRAP_TEST DFM_TEST_HOME DFM_TEST_OS DFM_TEST_ARCH DFM_TEST_CLT DFM_TEST_BEFORE_SWAP DFM_TEST_AFTER_SWAP DFM_TEST_BEFORE_COPY DFM_TEST_UID", t, " "); for (i in t) ok[t[i]] = 1 } }
  /^[^=]+=/ { if (!($1 in ok)) { print "unexpected " $1; bad = 1; exit } }
  END { if (!bad) print "clean" }') || env_check=
case $env_check:$PATH in
  clean:/usr/bin:/bin:/usr/sbin:/sbin) ;;
  *)
    /usr/bin/printf 'error: E_ENV_UNEXPECTED the environment holds more than the script allows (%s); start it as /bin/sh scripts/bootstrap.sh\n' "${env_check:-unreadable}" >&2
    exit 3 ;;
esac
set -eu
umask 077
unset CDPATH
SYSPATH=/usr/bin:/bin:/usr/sbin:/sbin # same list as dfm_platform_macos::SYSTEM_PATH
PATH=$SYSPATH
export PATH LC_ALL=C
DATA_REL=.local/share/dotfiles-manager # same literal as dfm_platform_macos::DATA_DIR
CACHE_REL=.cache/dotfiles-manager      # same literal as dfm_platform_macos::CACHE_DIR
ORIGIN=https://github.com/stevencarpenter/dotfiles-manager.git # docs/architecture.md section 12, item 3.1
TAB=$(printf '\t')
NL='
'

# Exit 3 means nothing was changed. install sets refuse_code=1 at its first change to the data directory.
refuse_code=3
refuse() { printf 'error: %s %s\n' "$1" "$2" >&2; exit "$refuse_code"; }
usage() { printf 'error: E_USAGE %s\n' "$1" >&2; exit 2; }
# $1 is awk output that starts with an error code.
refuse_out() { [ -z "$1" ] || printf 'error: %s\n' "$1" >&2; exit "$refuse_code"; }

# Test hooks exist only when DFM_BOOTSTRAP_TEST=1 and replace the dscl and uname answers.
test_mode() { [ "${DFM_BOOTSTRAP_TEST:-}" = 1 ]; }

# The user id that must own every directory the manager uses and every toolchain entry. Test mode may
# name another one (DFM_TEST_UID), because a test cannot create a file owned by another user.
owner_uid() { if test_mode && [ -n "${DFM_TEST_UID:-}" ]; then printf '%s\n' "$DFM_TEST_UID"; else /usr/bin/id -u; fi; }

resolve_home() {
  if test_mode && [ -n "${DFM_TEST_HOME:-}" ]; then
    pw_home=$DFM_TEST_HOME
  else
    user=$(/usr/bin/id -un)
    pw_home=$(/usr/bin/dscl . -read "/Users/$user" NFSHomeDirectory 2>/dev/null | /usr/bin/sed -n 's/^NFSHomeDirectory: //p') || pw_home=
  fi
  [ -n "$pw_home" ] || refuse E_HOME_MISMATCH "cannot read the home directory from the password database"
  case $pw_home in *[[:space:][:cntrl:]]*) refuse E_HOME_MISMATCH "the home directory contains whitespace or control characters; this is not supported" ;; esac
  # LC_ALL=C makes [:space:] ASCII only. The CLI (valid_home) also refuses Unicode White_Space and the C1
  # controls, matched here on the UTF-8 bytes: U+0080..U+00A0, U+1680, U+2000..U+200A, U+2028, U+2029,
  # U+202F, U+205F, U+3000.
  hex=$(printf '%s' "$pw_home" | /usr/bin/od -An -tx1 -v | /usr/bin/tr -s ' \n' '  ')
  case $hex in
    *" c2 8"[0-9a-f]" "*|*" c2 9"[0-9a-f]" "*|*" c2 a0 "*|*" e1 9a 80 "*|*" e2 80 8"[0-9a]" "*|*" e2 80 a8 "*|*" e2 80 a9 "*|*" e2 80 af "*|*" e2 81 9f "*|*" e3 80 80 "*)
      refuse E_HOME_MISMATCH "the home directory contains whitespace or control characters; this is not supported" ;;
  esac
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
    for (i = 1; i <= n; i++) if (s[i] == "dependencies" || s[i] == "dev-dependencies" || s[i] == "build-dependencies" || s[i] == "dev_dependencies" || s[i] == "build_dependencies") { k = i; break }
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
  split("patch replace dependencies dev-dependencies build-dependencies dev_dependencies build_dependencies target workspace", a, " ")
  for (i in a) DENY[a[i]] = 1
}
END { if (failed) exit 3 }
'

# Early, read-only pin hint: the same table-aware reading of [manager].toolchain as LOCK_RULES,
# without the demand for the other keys (the staged check makes that demand later).
PIN_HINT_RULES='
function on_header(name, arr) { if (name == "manager" && arr) bad("[[manager]] is an array of tables"); TABLE = name }
function on_inline(k, sk) { bad("inline table in lock") }
function on_pair(key) {
  if (TABLE == "manager" && key == "toolchain") {
    if (H != "" || VK != "s" || VS !~ /^[0-9]+\.[0-9]+\.[0-9]+$/) bad("invalid or repeated toolchain")
    H = VS
  }
}
END { if (failed) exit 3; if (H != "") print H }
'

LOCK_RULES='
function on_header(name, arr) {
  if (name == "manager") { if (arr) bad("[[manager]] is an array of tables"); if (MSEEN) bad("duplicate [manager]"); MSEEN = 1 }
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
  bad=$(cd "$stage" && { find . -name "*$NL*" -print; find . -print | grep '[^ -~]\|\\' || true; } | head -n 1)
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
  ! find . -print | grep -q '[^ -~]\|\\' || exit 1
  # A find that fails prints nothing, so the status is tested too: a failure never reads as "no match".
  # The digest records only the owner-execute bit, so group- or world-writable entries are refused.
  out=$(find . ! -type l \( -perm -020 -o -perm -002 \) -print) && [ -z "$out" ] || exit 1
  # Another owner could rewrite an entry after the digest, so every entry (a link too) must belong to the current user.
  uid=$(owner_uid) && [ -n "$uid" ] || exit 1
  out=$(find . ! -user "$uid" -print) && [ -z "$out" ] || exit 1
  nf=$(find . -type f | wc -l); nl=$(find . -type l | wc -l)
  [ "$nf" -gt 0 ] || exit 1
  plain=$(find . -type f ! -perm -u+x -print0 | xargs -0 -r /usr/bin/shasum -a 256 |
    awk '{ h = $1; sub(/^[0-9a-f]+  \.\//, ""); printf "%s\tfile\t%s\n", $0, h }')
  execs=$(find . -type f -perm -u+x -print0 | xargs -0 -r /usr/bin/shasum -a 256 |
    awk '{ h = $1; sub(/^[0-9a-f]+  \.\//, ""); printf "%s\texec\t%s\n", $0, h }')
  # `readlink -n` prints the target bytes and nothing else, and the `x` keeps $(...) from dropping
  # a trailing newline of the target. A target that holds a newline becomes an empty target,
  # which the awk program below refuses, as the Rust checker refuses a non-printable byte.
  links=$(find . -type l | while IFS= read -r l; do
      t=$(readlink -n "$l"; printf x); t=${t%x}
      [ "${t#*"$NL"}" = "$t" ] || t=
      printf '%s\t%s\n' "${l#./}" "$t"
    done |
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
  [ -n "$PIN" ] && [ -n "$TREE" ] && [ -n "$got" ] && [ "$got" = "$TREE" ] || {
    # cp -R into an existing destination nests the copy one level down.
    for _n in "$TC/$PIN" "$TC/$PIN"-*; do
      [ ! -d "$_n" ] || refuse E_TOOLCHAIN_MISMATCH "toolchain $PIN tree digest differs from the lock; $_n is nested inside it (cp -R into an existing directory). Remove $TC and copy the toolchain to $TC again"
    done
    refuse E_TOOLCHAIN_MISMATCH "toolchain $PIN tree digest differs from the lock"
  }
}

controls_ancestors() { # uses CACHE
  d=$CACHE/build; tail=
  while [ ! -d "$d" ]; do tail=/${d##*/}$tail; d=$(dirname "$d"); done
  base=$(cd "$d" 2>/dev/null && pwd -P) || refuse E_BUILD_CONFIG "cannot resolve the build directory $d"
  [ "$base" = / ] && base=
  d=$base$tail; [ -n "$d" ] || d=/
  while :; do
    # An ancestor that cannot be searched already failed the cd above, so every d here is searchable.
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
# The script never starts the Command Line Tools installer: that is a long, interactive
# change to the machine. It refuses before any mutation and tells the owner what to do.
# Command Line Tools keep clang at usr/bin; a full Xcode keeps it in its default toolchain.
ensure_clt() {
  clt=$(/usr/bin/xcode-select -p 2>/dev/null) || clt=
  if { test_mode && [ "${DFM_TEST_CLT:-}" = missing ]; } || [ -z "$clt" ] ||
    { [ ! -x "$clt/usr/bin/clang" ] && [ ! -x "$clt/Toolchains/XcodeDefault.xctoolchain/usr/bin/clang" ]; }; then
    refuse E_PREREQ_MISSING "Command Line Tools are missing. Run 'xcode-select --install', wait for it to finish, then run this script again"
  fi
}

# `current` and `current.new` are only ever symlinks that the installer creates. Each leaf is tested
# without following links: absent or a symlink passes, anything else (directory, file, FIFO) is
# refused, so no recursive removal ever runs on a name the installer does not own.
check_current_leaves() {
  for _l in "$DATA/current" "$DATA/current.new"; do
    if [ ! -L "$_l" ] && [ -e "$_l" ]; then
      refuse E_PREREQ_MISSING "$_l exists and is not a symlink; move it away first"
    fi
  done
}

# The command path may only be replaced when it is absent or already ours, and `current`
# may only be replaced when it is absent or a symlink. Checked early and again before the swap.
check_command_path() {
  check_current_leaves
  link=$HOME_DIR/.local/bin/dfm
  if [ -L "$link" ]; then
    [ "$(readlink "$link")" = "$DATA/current/dfm" ] ||
      refuse E_PREREQ_MISSING "$link is a symlink that points elsewhere; move it away first"
  elif [ -e "$link" ]; then
    refuse E_PREREQ_MISSING "$link exists and is not a symlink; move it away first"
  fi
}

# check_command_path proved that the command path is absent or already this manager's link.
# An existing link resolves through `current` and is left alone. An absent one is created from a
# link inside a directory that mktemp -d just made, so no name that existed before is touched,
# and is renamed into place. A link that exists before `current` does is harmless: it dangles.
ensure_command_link() {
  link=$HOME_DIR/.local/bin/dfm; want=$DATA/current/dfm
  [ ! -L "$link" ] || return 0
  check_bin_dir
  link_dir=$(mktemp -d "$HOME_DIR/.local/bin/.dfm-link.XXXXXX") || { echo "error: cannot stage the command link" >&2; exit 1; }
  ln -s "$want" "$link_dir/link" || { rm -rf "$link_dir"; echo "error: cannot stage the command link" >&2; exit 1; }
  # Test the destination again right before the rename: mv would move the link into a directory
  # that appeared since the first check, or replace a link that is not ours.
  if [ -L "$link" ]; then
    [ "$(readlink "$link")" = "$want" ] || { rm -rf "$link_dir"; refuse E_PREREQ_MISSING "$link appeared and points elsewhere; move it away first"; }
  elif [ -e "$link" ]; then
    rm -rf "$link_dir"; refuse E_PREREQ_MISSING "$link appeared and is not a symlink; move it away first"
  else
    mv -fh "$link_dir/link" "$link" || { rm -rf "$link_dir"; echo "error: cannot create the command link" >&2; exit 1; }
  fi
  rm -rf "$link_dir"
  [ "$(readlink "$link")" = "$want" ] || { echo "error: the command link is '$(readlink "$link" || true)', expected '$want'" >&2; exit 1; }
}

# The CLI opens the binding through open_config: HOME, then .config and .config/dotfiles-manager,
# each opened without following a link, owned by the current user and closed to group and others
# (mode & 022 == 0). Mirror those rules on every component that exists. A missing component is fine:
# the CLI creates it. HOME itself may be reached through an alias, as in open_home.
check_config_tree() {
  _uid=$(/usr/bin/id -u)
  for _p in "$HOME_DIR" "$HOME_DIR/.config" "$HOME_DIR/.config/dotfiles-manager"; do
    if [ "$_p" != "$HOME_DIR" ]; then
      { [ -e "$_p" ] || [ -L "$_p" ]; } || return 0
      [ ! -L "$_p" ] || refuse E_PREREQ_MISSING "$_p is a symlink; the CLI refuses a binding reached through it. Move it away first"
      [ -d "$_p" ] || refuse E_PREREQ_MISSING "$_p exists and is not a directory; move it away first"
    fi
    _t=$_p; [ "$_p" != "$HOME_DIR" ] || _t=$_p/. # HOME is resolved first, so its target is checked
    [ "$(/usr/bin/stat -f %u "$_t")" = "$_uid" ] ||
      refuse E_PREREQ_MISSING "$_p is not owned by the current user; the CLI refuses a binding below it"
    _m=$(/usr/bin/stat -f %Lp "$_t")
    [ $(( 0$_m & 022 )) -eq 0 ] ||
      refuse E_PREREQ_MISSING "$_p has mode $_m, writable by group or others; the CLI refuses a binding below it"
  done
}
# uninstall removes trees below HOME, so HOME itself must be closed to other users too: the same
# owner and mode rule that check_config_tree applies to it, without the .config components.
check_home_dir() {
  [ "$(/usr/bin/stat -f %u "$HOME_DIR/.")" = "$(owner_uid)" ] ||
    refuse E_PREREQ_MISSING "$HOME_DIR is not owned by the current user; move the manager's trees away first"
  _m=$(/usr/bin/stat -f %Lp "$HOME_DIR/.")
  [ $(( 0$_m & 022 )) -eq 0 ] ||
    refuse E_PREREQ_MISSING "$HOME_DIR has mode $_m, writable by group or others; run chmod go-w on it first"
}

# An existing binding must be usable by the new binary exactly as it is: a regular file, not a
# symlink or FIFO, mode 0600, owned by the current user, a single hard link (the rules of read_binding in the CLI).
check_binding() {
  b=$HOME_DIR/.config/dotfiles-manager/machine.toml
  { [ -e "$b" ] || [ -L "$b" ]; } || return 0
  [ ! -L "$b" ] || binding_bad "$b" "is a symlink"
  [ -f "$b" ] || binding_bad "$b" "is not a regular file"
  [ "$(/usr/bin/stat -f %Lp "$b")" = 600 ] || binding_bad "$b" "mode is not 0600"
  [ "$(/usr/bin/stat -f %u "$b")" = "$(/usr/bin/id -u)" ] || binding_bad "$b" "is not owned by the current user"
  [ "$(/usr/bin/stat -f %l "$b")" = 1 ] || binding_bad "$b" "has more than one hard link"
}
binding_bad() { printf 'error: E_BINDING_INVALID %s %s\n' "$1" "$2" >&2; exit "$refuse_code"; }

# $1 is a path below HOME. Refuses when an existing component of it is a symlink, not a
# directory, not owned by the current user or writable by group or others (mode & 022), so no
# recursive chmod or rm and no copy can reach another tree through it, and no other user can
# rename a component. Components that do not exist yet are fine: mkdir creates them. HOME
# itself is not checked here (check_config_tree does for install, check_home_dir for uninstall).
# A same-user process can still swap a component after the check; see docs/bootstrap.md.
check_private_path() {
  _p=$HOME_DIR _r=$1
  while [ -n "$_r" ]; do
    case $_r in */*) _c=${_r%%/*}; _r=${_r#*/} ;; *) _c=$_r; _r= ;; esac
    _p=$_p/$_c
    if [ -L "$_p" ]; then
      refuse E_PREREQ_MISSING "$_p is a symlink; the manager does not follow links in its own directories. Move it away first"
    fi
    [ -e "$_p" ] || return 0
    [ -d "$_p" ] || refuse E_PREREQ_MISSING "$_p exists and is not a directory; move it away first"
    [ "$(/usr/bin/stat -f %u "$_p")" = "$(owner_uid)" ] ||
      refuse E_PREREQ_MISSING "$_p is not owned by the current user; move it away first"
    _m=$(/usr/bin/stat -f %Lp "$_p")
    [ $(( 0$_m & 022 )) -eq 0 ] ||
      refuse E_PREREQ_MISSING "$_p has mode $_m, writable by group or others; run chmod go-w on it first"
  done
}
check_cache_tree() { check_private_path "$CACHE_REL/build"; }
# The bin directory receives the command link: a real directory the current user owns.
check_bin_dir() { check_private_path .local/bin; }
check_data_tree() {
  for _d in toolchains versions cargo-home rustup-home build-home; do check_private_path "$DATA_REL/$_d"; done
}

rm_build() { [ ! -e "$build" ] || chmod -R u+w "$build"; rm -rf "$build"; }

# Replacement refs (refs/replace) would let a ref swap the content of the verified commit.
# core.autocrlf and core.eol in the checkout's config would rewrite line endings in git archive, so
# the command line pins them (it outranks every config file). Checkout-local attributes and filters
# cannot be pinned by name: cmd_install refuses a checkout that has them.
git_clean() { /usr/bin/env -i PATH="$SYSPATH" HOME=/var/empty LANG=C GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null \
  GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0 GIT_NO_REPLACE_OBJECTS=1 /usr/bin/git -c core.fsmonitor=false \
  -c core.hooksPath=/dev/null -c core.useReplaceRefs=false -c core.autocrlf=false -c core.eol=lf "$@"; }

# Removes the build directory and the build homes. Each tree goes only when its own check passes,
# so a link planted since the last check is never followed. Sets `left` to what stayed and why.
clean_build() {
  left=
  if why=$( (check_cache_tree) 2>&1 ); then rm_build || left=$build; else left="$build ($why)"; fi
  for _h in cargo-home rustup-home build-home; do
    if why=$( (check_private_path "$DATA_REL/$_h") 2>&1 ); then
      rm -rf "${DATA:?}/$_h" || left="$left; $DATA/$_h"
    else left="$left; $DATA/$_h ($why)"; fi
  done
  left=${left#; }
}
# Armed once install first changes the build directories; a run that did not finish removes them.
install_unfinished() {
  [ -z "${install_ok:-}" ] || return 0
  clean_build || :
  for _d in "$CACHE" "$HOME_DIR/.cache"; do
    case " ${made_dirs:-} " in *" $_d "*) rmdir "$_d" 2>/dev/null || : ;; esac
  done
  [ -z "$left" ] || echo "bootstrap: install did not finish; left in place: $left" >&2
}

# Read-only checks of a git repository's own configuration at $1. git archive would apply
# checkout-local attributes (export-ignore, export-subst) and run filter commands from the
# checkout's config, so the staged tree could differ from the pinned commit. The build reads a
# fresh bare clone, which has neither, but a local checkout that carries them is still refused
# before anything is written. Attributes committed in the pinned tree stay part of that commit;
# no filter can run for them. A failed git call refuses: an empty answer must never read as
# "nothing found".
check_checkout_config() {
  [ -d "$1" ] || refuse E_SOURCE_UNTRUSTED "source $1 is not a directory"
  rc=0; found=$(git_clean -C "$1" config --get-regexp '^(core\.attributesfile|filter\.)' 2>/dev/null) || rc=$?
  [ "$rc" -le 1 ] || refuse E_SOURCE_UNTRUSTED "cannot read the git configuration of the source checkout (git exit $rc)"
  [ -z "$found" ] || refuse E_SOURCE_UNTRUSTED "the source checkout configures core.attributesFile or a filter; remove it first"
  attrs=$(git_clean -C "$1" rev-parse --path-format=absolute --git-path info/attributes 2>/dev/null) && [ -n "$attrs" ] ||
    refuse E_SOURCE_UNTRUSTED "cannot read the git directory of the source checkout"
  [ ! -s "$attrs" ] || refuse E_SOURCE_UNTRUSTED "the source checkout has .git/info/attributes; remove it first"
}

# Identity checks of the bare clone at $src: reads fmt and the pin, sets hint (the toolchain pin).
check_source() {
  check_checkout_config "$src"
  [ "$(git_clean -C "$src" rev-parse --show-object-format 2>/dev/null)" = "$fmt" ] ||
    refuse E_SOURCE_UNTRUSTED "the source checkout does not use the $fmt object format of the pin"
  resolved=$(git_clean -C "$src" rev-parse --verify --end-of-options "$commit^{commit}" 2>/dev/null) || resolved=
  [ "$resolved" = "$commit" ] || refuse E_SOURCE_UNTRUSTED "$commit is not a commit in the source checkout"
  # The file that runs must be the script of the pinned commit: a stale, edited or foreign copy
  # of this script never builds a commit it does not belong to. The digest of $0 was taken
  # before any command ran. (A hostile script can skip its own checks; the commit id and the
  # out-of-band --expect-sha256 value are the owner's anchors, see docs/bootstrap.md.)
  # The type is checked first: without pipefail a failed cat-file would digest empty input.
  [ "$(git_clean -C "$src" cat-file -t "$commit:scripts/bootstrap.sh" 2>/dev/null)" = blob ] ||
    refuse E_SOURCE_UNTRUSTED "commit $commit has no scripts/bootstrap.sh file"
  committed=$(git_clean -C "$src" cat-file blob "$commit:scripts/bootstrap.sh" 2>/dev/null | /usr/bin/shasum -a 256) && committed=${committed%% *} || committed=
  [ -n "$committed" ] || refuse E_SOURCE_UNTRUSTED "cannot digest scripts/bootstrap.sh of commit $commit"
  [ "$committed" = "$self_sha" ] ||
    refuse E_SOURCE_UNTRUSTED "this script ($script_path, sha256 $self_sha) is not scripts/bootstrap.sh of commit $commit (sha256 $committed)"
  # Early hint of the pin so a missing toolchain is refused before any change outside the scratch
  # build directory. The real parse and every other check run later on the staged copy.
  hint=$(git_clean -C "$src" cat-file blob "$commit:dotfiles-manager.lock" 2>/dev/null |
    awk -v FN=dotfiles-manager.lock "$SCAN$PIN_HINT_RULES") || refuse_out "$hint"
  [ -n "$hint" ] || refuse E_SOURCE_UNTRUSTED "dotfiles-manager.lock has no toolchain pin"
  [ -d "$DATA/toolchains/$hint" ] ||
    refuse E_TOOLCHAIN_MISMATCH "toolchain $hint is absent at $DATA/toolchains/$hint; install exactly that version there (official Rust release distribution)"
}

cmd_install() {
  [ -n "${commit:-}" ] || usage "install needs --commit"
  # The pin is a full-length lowercase object id and nothing else: no abbreviation, ref, tag,
  # revision syntax (^, ~, @{}), option-like value or whitespace ever reaches git.
  case $commit in ""|*[!0-9a-f]*) refuse E_SOURCE_UNTRUSTED "--commit must be a full lowercase hex object id (40 or 64 digits)" ;; esac
  case ${#commit} in 40) fmt=sha1 ;; 64) fmt=sha256 ;; *) refuse E_SOURCE_UNTRUSTED "--commit must be a full lowercase hex object id (40 or 64 digits)" ;; esac
  [ -n "$src" ] || printf 'bootstrap: no --source given; the source is %s\n' "$ORIGIN" >&2
  resolve_home; check_platform
  # /usr/bin/git is the Command Line Tools shim, so the tools are probed before any git call.
  ensure_clt
  build=$CACHE/build
  # install resets the build directory, so neither this script nor a local source may live in it.
  # Paths are resolved (cd -P) before the comparison, so `..` segments and symlinked directories
  # cannot hide the location, and a symlink leaf is followed (a directory fully, a file one level);
  # only an existing build directory can hold anything.
  if build_real=$(cd -P "$build" 2>/dev/null && pwd -P); then
    for _p in "$script_path" "${src#file://}"; do
      [ -n "$_p" ] || continue
      _d=$(cd -P "${_p%/*}/" 2>/dev/null && pwd -P) || continue
      _r=${_d%/}/${_p##*/}
      if [ -d "$_r" ]; then _r=$(cd -P "$_r" 2>/dev/null && pwd -P) || continue
      elif [ -L "$_r" ]; then
        _t=$(readlink "$_r") || continue
        case $_t in /*) ;; *) _t=${_d%/}/$_t ;; esac
        _d=$(cd -P "${_t%/*}/" 2>/dev/null && pwd -P) || continue
        _r=${_d%/}/${_t##*/}
      fi
      case $_r in "$build_real"|"$build_real/"*) refuse E_PREREQ_MISSING "$_p is inside the build directory $build, which install resets; move it elsewhere first" ;; esac
    done
  fi
  # Every source is read through a bare clone with transfer.fsckObjects=true: git does not verify a
  # loose object's hash on read, so a replaced object under .git/objects of a local checkout would
  # otherwise serve another tree for the pinned commit id (git archive, cat-file and rev-parse all
  # trust it). A local path is cloned as file://, because a plain path copies objects without the
  # transfer checks. The clone is made after every local check, so the network is used only when
  # the install can proceed; a local checkout's own configuration is checked before that.
  case $src in
    "") clone_url=$ORIGIN ;;
    https://*|file://*) clone_url=$src ;;
    *://*) refuse E_SOURCE_UNTRUSTED "--source must be a local path, an https:// URL or a file:// URL" ;;
    # git percent-decodes a file:// URL, so a % in the path is encoded.
    *) check_checkout_config "$src"; clone_url=file://$(printf '%s' "$src" | sed 's/%/%25/g') ;;
  esac
  # An https:// URL with userinfo is printed without it.
  case $clone_url in https://*@*) shown_url=https://${clone_url#*@} ;; *) shown_url=$clone_url ;; esac
  check_command_path
  check_config_tree; check_binding
  check_cache_tree; check_data_tree; check_bin_dir
  # A run that does not finish removes the cache directories it created, so a refusal after this
  # point leaves nothing behind (rmdir removes only an empty directory).
  made_dirs=
  for _d in "$HOME_DIR/.cache" "$CACHE"; do [ -d "$_d" ] || made_dirs="$made_dirs $_d"; done
  trap install_unfinished EXIT; trap 'exit 130' INT; trap 'exit 143' TERM
  rm_build; mkdir -p "$build/stage"
  # A bare clone into the scratch build directory: the pinned commit is read from it, and the exit
  # trap, the cleanup after the build and uninstall remove it with the rest of the build directory.
  # No working tree is written: git archive reads the commit itself, so there is no checkout head
  # to drift from the pin.
  git_clean -c transfer.fsckObjects=true clone --bare --quiet -- "$clone_url" "$build/source.git" ||
    refuse E_SOURCE_UNTRUSTED "cannot clone $shown_url (a source whose objects fail git's transfer checks is refused)"
  src=$build/source.git
  check_source
  # Build the verified commit itself, not whatever HEAD is. Both steps are checked: a pipe would hide
  # the status of git archive.
  git_clean -C "$src" archive --format=tar --end-of-options "$commit" >"$build/source.tar" || { echo "error: git archive failed" >&2; exit 1; }
  tar -xf "$build/source.tar" -C "$build/stage" || { echo "error: cannot extract the source" >&2; exit 1; }
  rm -f "$build/source.tar"
  controls "$build/stage"
  chmod -R a-w "$build/stage" # same-uid writers can still undo this; see docs/bootstrap.md
  # Controls passed. From here the pinned toolchain runs under a constructed environment.
  check_cache_tree; check_data_tree # again: the window since the first check was the staging and the controls
  refuse_code=1 # the first change outside the scratch build directory is next; refusals after it are failures
  for d in cargo-home rustup-home build-home; do rm -rf "${DATA:?}/$d"; mkdir -p "$DATA/$d"; done
  cd "$build"
  tcrun() { /usr/bin/env -i PATH="$TC/bin:$SYSPATH" HOME="$DATA/build-home" CARGO_HOME="$DATA/cargo-home" \
    RUSTUP_HOME="$DATA/rustup-home" RUSTC="$TC/bin/rustc" LANG=C DFM_BUILD_COMMIT="$commit" "$@"; }
  # Check the toolchain again immediately before it first runs.
  got=$(toolchain_digest "$TC") || got=
  [ -n "$got" ] && [ "$got" = "$TREE" ] || refuse E_TOOLCHAIN_MISMATCH "toolchain $PIN changed after the check"
  tcrun "$TC/bin/cargo" fetch --locked --manifest-path "$build/stage/Cargo.toml" || { echo "error: cargo fetch failed" >&2; exit 1; }
  tcrun "$TC/bin/cargo" build --release --locked --offline --manifest-path "$build/stage/Cargo.toml" \
    --target-dir "$build/target" -p dfm-cli || { echo "error: cargo build failed" >&2; exit 1; }
  bin=$build/target/release/dfm
  # The new binary must be able to read the binding that already exists, so it checks a copy.
  mkdir -p "$build/fixture"
  if test_mode && [ -n "${DFM_TEST_BEFORE_COPY:-}" ]; then /bin/sh "$DFM_TEST_BEFORE_COPY"; fi
  check_config_tree; check_binding
  if [ -e "$HOME_DIR/.config/dotfiles-manager/machine.toml" ]; then
    cp -p "$HOME_DIR/.config/dotfiles-manager/machine.toml" "$build/fixture/machine.toml"
  fi
  tcrun "$bin" self-check --fixture-dir "$build/fixture" >/dev/null || { echo "error: self-check failed" >&2; exit 1; }
  ver=$(tcrun "$bin" --version) || { echo "error: the built binary did not run" >&2; exit 1; }
  case $ver in "dfm "*" (commit $commit)") ;; *) echo "error: the built binary reports a different identity: $ver" >&2; exit 1 ;; esac
  semver=${ver#dfm }; semver=${semver%% *}
  case $semver in ""|*[!0-9.]*) echo "error: unexpected version $semver" >&2; exit 1 ;; esac
  name=$semver-$(printf '%s' "$commit" | cut -c1-12)
  # Every existing component of versions/<name> must be a real directory. The binary is copied
  # to a file that mktemp creates exclusively (never through a link), then renamed into place.
  check_private_path "$DATA_REL/versions/$name"
  mkdir -p "$DATA/versions/$name" "$HOME_DIR/.local/bin"
  check_private_path "$DATA_REL/versions/$name"
  [ ! -d "$DATA/versions/$name/dfm" ] ||
    refuse E_PREREQ_MISSING "$DATA/versions/$name/dfm is a directory; move it away first"
  staged_bin=$(mktemp "$DATA/versions/$name/dfm.new.XXXXXX") || { echo "error: cannot create the staged binary" >&2; exit 1; }
  { cp "$bin" "$staged_bin" && chmod 755 "$staged_bin" && mv -f "$staged_bin" "$DATA/versions/$name/dfm"; } ||
    { rm -f "$staged_bin"; echo "error: cannot stage the binary" >&2; exit 1; }
  if test_mode && [ -n "${DFM_TEST_BEFORE_SWAP:-}" ]; then /bin/sh "$DFM_TEST_BEFORE_SWAP"; fi
  check_command_path
  check_config_tree; check_binding
  # The command link is the only step in a directory the manager does not own, so it comes before
  # the flip. After the flip only the read-back and the cleanup remain.
  ensure_command_link
  # check_command_path proved current.new is absent or a symlink; a stale link is removed first.
  rm -f "$DATA/current.new"; ln -sfn "versions/$name" "$DATA/current.new"; mv -fh "$DATA/current.new" "$DATA/current"
  if test_mode && [ -n "${DFM_TEST_AFTER_SWAP:-}" ]; then /bin/sh "$DFM_TEST_AFTER_SWAP"; fi
  # Read both links back: a swap that did not take effect must not be reported as done.
  got_current=$(readlink "$DATA/current") || got_current=
  got_link=$(readlink "$HOME_DIR/.local/bin/dfm") || got_link=
  if [ "$got_current" != "versions/$name" ]; then
    echo "error: swap did not take effect: $DATA/current is '$got_current', expected 'versions/$name'" >&2; exit 1
  fi
  if [ "$got_link" != "$DATA/current/dfm" ]; then
    echo "error: swap did not take effect: the command path is '$got_link', expected '$DATA/current/dfm'" >&2; exit 1
  fi
  retire_old_command_link
  # Again: the window since the last check was the whole build. The swap is done, so a failed
  # check is an exit 1 after cleaning what is safe to clean, never a refusal with exit 3.
  clean_build
  if [ -n "$left" ]; then
    install_ok=1
    echo "error: the swap took effect and $DATA/current is the new version, but a directory check failed afterwards; not removed: $left. Fix the directory and remove it yourself" >&2; exit 1
  fi
  install_ok=1
  sha=$(/usr/bin/shasum -a 256 "$DATA/versions/$name/dfm" | cut -d' ' -f1)
  printf 'installed %s\nsource commit %s\nbinary sha256 %s (record only)\nbootstrap: done\n' "$DATA/versions/$name" "$commit" "$sha"
}

# The command was `dotfiles-manager` before DFM-67. A link of that name that points into this
# manager's `current` is removed after the swap and by uninstall; anything else there is named
# and left alone. A later release may drop this rule.
retire_old_command_link() {
  old=$HOME_DIR/.local/bin/dotfiles-manager
  if [ -L "$old" ]; then
    case $(readlink "$old") in
      "$DATA/current/"*) rm -f "$old" && echo "removed the old command link $old" ;;
      *) echo "note: $old is a symlink that is not this manager's; left alone" ;;
    esac
  elif [ -e "$old" ]; then
    echo "note: $old exists and is not this manager's link; left alone"
  fi
}

# Removes what install created. The toolchain, state, binding and anything else in the cache
# root are never touched.
cmd_uninstall() {
  resolve_home
  check_home_dir; check_cache_tree; check_data_tree; check_current_leaves; check_bin_dir
  link=$HOME_DIR/.local/bin/dfm
  if [ -L "$link" ] && [ "$(readlink "$link")" = "$DATA/current/dfm" ]; then rm -f "$link"; fi
  retire_old_command_link
  build=$CACHE/build; rm_build
  rm -f "$DATA/current" "$DATA/current.new"
  rm -rf "${DATA:?}/versions" "$DATA/cargo-home" "$DATA/rustup-home" "$DATA/build-home"
  rmdir "$CACHE" "$DATA" 2>/dev/null || true # ~/.local/bin is not ours to remove
  printf 'removed the installed manager; toolchains, state and binding were left in place\n'
}

cmd=${1:-}; [ $# -gt 0 ] && shift
src='' commit='' dir='' expect='' seen=' '
# A flag is accepted once, and only by the commands that use it.
flag() {
  case $cmd in
    install) ok=' --source --commit --expect-sha256 ' ;;
    check-controls) ok=' --source --expect-sha256 ' ;;
    toolchain-digest) ok=' --dir --expect-sha256 ' ;;
    uninstall) ok=' --expect-sha256 ' ;;
    *) ok=' --source --commit --dir --expect-sha256 ' ;; # unknown command: the final usage line explains
  esac
  case $ok in *" $1 "*) ;; *) usage "$cmd does not use $1" ;; esac
  case $seen in *" $1 "*) usage "$1 given twice" ;; esac
  seen="$seen$1 "
}
while [ $# -gt 0 ]; do
  case $1 in
    --source) flag --source; [ $# -ge 2 ] || usage "--source needs a value"; src=$2; shift 2 ;;
    --commit) flag --commit; [ $# -ge 2 ] || usage "--commit needs a value"; commit=$2; shift 2 ;;
    --dir) flag --dir; [ $# -ge 2 ] || usage "--dir needs a value"; dir=$2; shift 2 ;;
    --expect-sha256) flag --expect-sha256; [ $# -ge 2 ] || usage "--expect-sha256 needs a value"; expect=$2; shift 2 ;;
    *) usage "unknown argument $1" ;;
  esac
done
# An empty --source is a usage error; only an absent one means the canonical origin.
case $seen in *" --source "*) [ -n "$src" ] || usage "--source needs a non-empty value" ;; esac
# Relative arguments become absolute so a later cd cannot be redirected. A URL stays as it is.
case $src in ""|/*|*://*) ;; *) src=$PWD/$src ;; esac
case $dir in ""|/*) ;; *) dir=$PWD/$dir ;; esac
case $0 in /*) script_path=$0 ;; *) script_path=$PWD/$0 ;; esac
# The script's own identity, before any command runs: the digest of this file as it executes.
# --expect-sha256 is the value the owner holds out of band (contract section 12, item 3.1);
# install also compares the digest with scripts/bootstrap.sh of the pinned commit (check_source).
self_sha=$(/usr/bin/shasum -a 256 "$script_path") && self_sha=${self_sha%% *} || self_sha=
[ -n "$self_sha" ] || refuse E_SOURCE_UNTRUSTED "cannot digest this script at $script_path; run it as /bin/sh <path to bootstrap.sh>"
if [ -n "$expect" ]; then
  case $expect in *[!0-9a-f]*) usage "--expect-sha256 must be 64 lowercase hex digits" ;; esac
  [ ${#expect} -eq 64 ] || usage "--expect-sha256 must be 64 lowercase hex digits"
  [ "$self_sha" = "$expect" ] || refuse E_SOURCE_UNTRUSTED "this script ($script_path) has sha256 $self_sha, expected $expect"
fi
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
  *) usage "usage: bootstrap.sh install [--source <git checkout or URL>] --commit <sha> | check-controls --source <tree> | uninstall" ;;
esac
