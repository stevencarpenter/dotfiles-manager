//! The `brew` and `mise` look-alikes the package and trial tests plant on `DFM_PROVIDER_PATH`.
//! Each logs its arguments and the provider variables it saw to `../log`, lists what the fixture
//! files beside it name, and installs by creating the same entries, so no real provider runs.
//! `crates/cli/tests/packages.rs` keeps its own copy of these scripts.

use std::path::Path;

/// `brew --cellar` and `brew --caskroom` print the fixture's `Cellar` and `Caskroom` (`--caskroom`
/// fails with the `caskroom-fail` marker), where an
/// installed package is a `<name>/<version>` directory (the `installed` helper and `brew
/// install` create them); there is no `brew list`, so a probe that calls it fails. `brew deps
/// <name>` answers from `deps-<name>`; `brew outdated --json=v2 <name>...` reports the names in
/// `outdated-brew` and exits 1 when there is one; `brew ruby -e <script> <name>...` answers
/// `<name> 1` (brew pours a bottle) for every name except those in `nobottle-brew`, which answer
/// `<name> 0`; there is no `brew info`. `brew install <name>` adds the
/// name and its missing dependencies, or does nothing with the `noop` marker, or fails with the
/// `fail` marker. `brew uninstall --formula|--cask [--force] <name>...` logs
/// `uninstall autoremove=<HOMEBREW_NO_AUTOREMOVE>` and removes the racks (a formula rack with
/// more than one version loses only its last one without `--force`), or refuses the whole call
/// (exit 1) when a `needed-<name>` file names a formula outside the call.
/// `brew formulae` prints `official-brew` (default: atuin and duckdb) and `brew
/// casks` prints `official-cask`. Every command takes a name pinned to its namespace
/// (`homebrew/core/<name>`, or `--cask <name>`) and answers with the short name, as brew does.
pub const FAKE_BREW: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
printf 'brew %s | auto=%s analytics=%s hints=%s cleanup=%s upgrade=%s dependents=%s user=%s\n' "$*" "${HOMEBREW_NO_AUTO_UPDATE:-unset}" "${HOMEBREW_NO_ANALYTICS:-unset}" "${HOMEBREW_NO_ENV_HINTS:-unset}" "${HOMEBREW_NO_INSTALL_CLEANUP:-unset}" "${HOMEBREW_NO_INSTALL_UPGRADE:-unset}" "${HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK:-unset}" "${USER:-none}" >> "$here/../log"
case "$1" in
  --cellar)
    [ -e "$here/../linger" ] && { (sleep 20 &); (sleep 0.3; kill -TERM $PPID) >/dev/null 2>&1 & }
    [ -e "$here/../stop" ] && kill -TERM $PPID
    echo "$(cd "$here/.." && pwd)/Cellar"; exit 0 ;;
  --caskroom)
    [ -e "$here/../caskroom-fail" ] && exit 1
    echo "$(cd "$here/.." && pwd)/Caskroom"; exit 0 ;;
  formulae)
    if [ -e "$here/../official-brew" ]; then cat "$here/../official-brew"; else printf 'atuin\nduckdb\n'; fi
    exit 0 ;;
  casks) cat "$here/../official-cask" 2>/dev/null; exit 0 ;;
  ruby)
    shift 3
    for n in "$@"; do
      n=${n##*/}
      if grep -qx "$n" "$here/../nobottle-brew" 2>/dev/null; then echo "$n 0"; else echo "$n 1"; fi
    done
    exit 0 ;;
  deps)
    n=$2; [ "$n" = --cask ] && n=$3
    cat "$here/../deps-${n##*/}" 2>/dev/null; exit 0 ;;
  outdated)
    shift 2; rc=0; sep=""; printf '{"formulae":['
    for n in "$@"; do
      n=${n##*/}
      if grep -qx "$n" "$here/../outdated-brew" 2>/dev/null; then
        printf '%s{"name":"%s"}' "$sep" "$n"; sep=","; rc=1
      fi
    done
    printf '],"casks":[]}\n'; exit $rc ;;
  install)
    [ -e "$here/../fail" ] && { echo "Error: PLANTED-STDERR" >&2; exit 1; }
    [ -e "$here/../noop" ] && exit 0
    root=Cellar; n=$2; [ "$n" = --cask ] && { root=Caskroom; n=$3; }; n=${n##*/}
    receipt() { printf '{"source":{"tap":"%s"}}' "$2" > "$1/INSTALL_RECEIPT.json"; }
    for d in $(cat "$here/../deps-$n" 2>/dev/null); do
      mkdir -p "$here/../Cellar/$d/1.0.0"; receipt "$here/../Cellar/$d/1.0.0" homebrew/core
    done
    if [ $root = Caskroom ]; then
      mkdir -p "$here/../Caskroom/$n/1.0.0" "$here/../Caskroom/$n/.metadata"
      receipt "$here/../Caskroom/$n/.metadata" homebrew/cask
    else
      mkdir -p "$here/../Cellar/$n/1.0.0"; receipt "$here/../Cellar/$n/1.0.0" homebrew/core
    fi
    echo "==> Installing $n"; exit 0 ;;
  uninstall)
    echo "uninstall autoremove=${HOMEBREW_NO_AUTOREMOVE:-unset}" >> "$here/../log"
    root=Cellar; [ "$2" = --cask ] && root=Caskroom; shift 2
    force=; [ "$1" = --force ] && { force=1; shift; }
    for n in "$@"; do
      for u in $(cat "$here/../needed-$n" 2>/dev/null); do
        case " $* " in *" $u "*) ;; *) echo "Error: Refusing to uninstall $n because it is required by $u" >&2; exit 1 ;; esac
      done
    done
    for n in "$@"; do
      d="$here/../$root/$n"
      if [ -z "$force" ] && [ "$root" = Cellar ] && [ "$(ls "$d" 2>/dev/null | wc -l)" -gt 1 ]; then
        rm -rf "$d/$(ls "$d" | tail -1)"
      else
        rm -rf "$d"
      fi
    done
    exit 0 ;;
  *) exit 64 ;;
esac
"#;

/// `mise ls --installed --json <name>` answers a JSON array from `installed-mise` (one
/// `<name> <version>` line per installed version); the table form is refused, as a stand-in for
/// its truncated tool column. `mise install <name>` appends; `mise uninstall --all <name>` drops every line of the name; `mise uninstall <name>@<v>...` drops those versions. A `mise-fail-<name>` marker beside
/// `bin` makes the install of that tool exit 1, as a failed download does.
pub const FAKE_MISE: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
printf 'mise %s | yes=%s auto=%s noconfig=%s\n' "$*" "${MISE_YES:-unset}" "${MISE_NOT_FOUND_AUTO_INSTALL:-unset}" "${MISE_NO_CONFIG:-unset}" >> "$here/../log"
db="$here/../installed-mise"
case "$1" in
  ls)
    [ "$2 $3" = "--installed --json" ] || { echo "the table truncates long names" >&2; exit 64; }
    name=$4
    v=${name##*@}
    case "$v" in */*) ;; *) [ "$v" != "$name" ] && { echo "mise ERROR $name not found in mise tool registry" >&2; exit 1; } ;; esac
    [ "$name" = "zzz-unknown" ] && { echo "mise ERROR $name not found in mise tool registry" >&2; exit 1; }
    sep=""; printf '['
    grep "^$name " "$db" 2>/dev/null | while read -r _ ver _; do
      printf '%s{"version":"%s","installed":true}' "$sep" "$ver"; sep=","
    done
    printf ']\n'
    exit 0 ;;
  install)
    [ -e "$here/../mise-fail-${2%%@*}" ] && { echo "mise ERROR download failed" >&2; exit 1; }
    echo "${2%%@*}  2.0.0" >> "$db"; exit 0 ;;
  uninstall)
    if [ "$2" = --all ]; then
      grep -v "^$3 " "$db" > "$db.new" 2>/dev/null; mv "$db.new" "$db"; exit 0
    fi
    shift
    for a in "$@"; do
      case "$a" in *@*) ;; *) exit 64 ;; esac
      grep -v "^${a%@*} *${a##*@}\$" "$db" > "$db.new" 2>/dev/null; mv "$db.new" "$db"
    done
    exit 0 ;;
  *) exit 64 ;;
esac
"#;

/// `gitleaks dir <path> ...` logs its arguments and the mode of the scanned directory to
/// `../log`. It exits 1 with the `gitleaks-fail` marker beside `bin`. Otherwise it reports every
/// file one level below the directory (`<i>/<name>`) that holds `FAKE_SECRET` as a JSON finding (the `File` is the path it was
/// given, as the real tool prints it) and exits 3, or prints `[]` and exits 0.
pub const FAKE_GITLEAKS: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
printf 'gitleaks %s\n' "$*" >> "$here/../log"
ls -ld "$2" | cut -c1-10 >> "$here/../log"
[ -e "$here/../gitleaks-fail" ] && { echo "PLANTED-STDERR" >&2; exit 1; }
[ -e "$here/../gitleaks-huge" ] && { head -c 17000000 /dev/zero; exit 3; }
rc=0; sep=""; printf '['
for f in "$2"/*/*; do
  [ -f "$f" ] || continue
  if grep -q FAKE_SECRET "$f"; then
    printf '%s{"RuleID":"fake","Match":"REDACTED","Secret":"REDACTED","File":"%s"}' "$sep" "$f"; sep=","; rc=3
  fi
done
printf ']\n'; exit $rc
"#;

/// `curl ... -o <tmp> -w %{url_effective} -- <url>` logs `curl <args>` to `../log`, writes
/// `../curl-body` to the `-o` path and prints `../curl-effective` (default: the URL) as the `-w`
/// format does. A nonzero `../curl-exit` makes it write and print nothing and exit with that code,
/// as a failed transfer does. A body larger than `--max-filesize` writes nothing and exits 63, as
/// curl does.
pub const FAKE_CURL: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
printf 'curl %s\n' "$*" >> "$here/../log"
out=; url=; max=
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out=$2; shift ;;
    --max-filesize) max=$2; shift ;;
    --) url=$2; break ;;
  esac
  shift
done
rc=$(cat "$here/../curl-exit" 2>/dev/null || echo 0)
[ "$rc" -ne 0 ] && exit "$rc"
[ -n "$max" ] && [ "$(wc -c < "$here/../curl-body")" -gt "$max" ] && exit 63
cat "$here/../curl-body" > "$out" || exit 23
if [ -e "$here/../curl-effective" ]; then cat "$here/../curl-effective"; else printf '%s' "$url"; fi
"#;

/// `sandbox-exec -p <profile> <command>...` logs `sandbox-exec <profile> | <command>` to `../log`
/// and runs the real `/usr/bin/sandbox-exec` with the same arguments. The `sandbox-missing`
/// marker makes it exit 127 instead.
pub const FAKE_SANDBOX_EXEC: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
[ -e "$here/../sandbox-missing" ] && exit 127
log() { p=$2; shift 2; printf 'sandbox-exec %s | %s\n' "$p" "$*" >> "$here/../log"; }
log "$@"
exec /usr/bin/sandbox-exec "$@"
"#;

/// `getconf <name>` logs `getconf <args>` to `../log` and prints `../tmpdir` when it exists, else
/// runs the real `/usr/bin/getconf`.
pub const FAKE_GETCONF: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
printf 'getconf %s\n' "$*" >> "$here/../log"
[ -e "$here/../tmpdir" ] && { cat "$here/../tmpdir"; exit 0; }
exec /usr/bin/getconf "$@"
"#;

/// `op whoami` exits 0 only with the `signed-in` marker and without a `signed-out-<account>`
/// marker, and kills itself with the `whoami-killed` marker; `op inject` fills the one reference.
pub const FAKE_OP: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
printf 'op %s\n' "$*" >> "$here/../log"
[ -e "$here/../whoami-killed" ] && kill -KILL $$
[ -e "$here/../signed-in" ] || { echo "[ERROR] no session" >&2; exit 1; }
for a in "$@"; do [ -e "$here/../signed-out-$a" ] && { echo "[ERROR] no session" >&2; exit 1; }; done
case "$1" in
  whoami) echo me; exit 0 ;;
  inject) sed 's|{{ op://vault/item/field }}|planted-value|' ;;
  *) exit 64 ;;
esac
"#;

/// A native tool look-alike: `--version` prints `st/<name>`, the update command writes the
/// `next-<name>` marker's content there (or exits 1 with the `fail-<name>` marker, or touches
/// `started-<name>` and sleeps with the `hang-<name>` marker). Every call is logged.
pub fn native_tool(name: &str, update: &str, st: &Path) -> String {
    format!(
        r#"#!/bin/sh
PATH=/usr/bin:/bin
st={st}
printf '{name} %s\n' "$*" >> "$st/log"
case "$1" in
  --version) cat "$st/{name}" ;;
  {update})
    [ -e "$st/hang-{name}" ] && {{ touch "$st/started-{name}"; exec sleep 30; }}
    [ -e "$st/tty-{name}" ] && {{ read x < /dev/tty || exit 9; }}
    [ -e "$st/fail-{name}" ] && {{ echo boom >&2; exit 1; }}
    [ -e "$st/next-{name}" ] && cp "$st/next-{name}" "$st/{name}"
    [ -e "$st/write-{name}" ] && echo changed >> "$(cat "$st/write-{name}")"
    exit 0 ;;
  plugin) exit 0 ;;
  *) exit 64 ;;
esac
"#,
        st = st.display()
    )
}
