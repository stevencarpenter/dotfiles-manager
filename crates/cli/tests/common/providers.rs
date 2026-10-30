//! The `brew` and `mise` look-alikes the package and trial tests plant on `DFM_PROVIDER_PATH`.
//! Each logs its arguments and the provider variables it saw to `../log`, lists what the fixture
//! files beside it name, and installs by creating the same entries, so no real provider runs.
//! `crates/cli/tests/packages.rs` keeps its own copy of these scripts.

/// `brew --cellar` and `brew --caskroom` print the fixture's `Cellar` and `Caskroom`, where an
/// installed package is a `<name>/<version>` directory (the `installed` helper and `brew
/// install` create them); there is no `brew list`, so a probe that calls it fails. `brew deps
/// <name>` answers from `deps-<name>`; `brew outdated --json=v2 <name>...` reports the names in
/// `outdated-brew` and exits 1 when there is one; `brew ruby -e <script> <name>...` answers
/// `<name> 1` (brew pours a bottle) for every name except those in `nobottle-brew`, which answer
/// `<name> 0`; there is no `brew info`. `brew install <name>` adds the
/// name and its missing dependencies, or does nothing with the `noop` marker, or fails with the
/// `fail` marker. `brew formulae` prints `official-brew` (default: atuin and duckdb) and `brew
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
  --caskroom) echo "$(cd "$here/.." && pwd)/Caskroom"; exit 0 ;;
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
  *) exit 64 ;;
esac
"#;

/// `mise ls --installed --json <name>` answers a JSON array from `installed-mise` (one
/// `<name> <version>` line per installed version); the table form is refused, as a stand-in for
/// its truncated tool column. `mise install <name>` appends.
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
    echo "${2%%@*}  2.0.0" >> "$db"; exit 0 ;;
  *) exit 64 ;;
esac
"#;

/// `gitleaks dir <path> ...` logs its arguments and the mode of the scanned directory to
/// `../log`. It exits 1 with the `gitleaks-fail` marker beside `bin`. Otherwise it reports every
/// file of the directory that holds `FAKE_SECRET` as a JSON finding (the `File` is the path it was
/// given, as the real tool prints it) and exits 3, or prints `[]` and exits 0.
pub const FAKE_GITLEAKS: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
printf 'gitleaks %s\n' "$*" >> "$here/../log"
ls -ld "$2" | cut -c1-10 >> "$here/../log"
[ -e "$here/../gitleaks-fail" ] && { echo "PLANTED-STDERR" >&2; exit 1; }
rc=0; sep=""; printf '['
for f in "$2"/*; do
  [ -f "$f" ] || continue
  if grep -q FAKE_SECRET "$f"; then
    printf '%s{"RuleID":"fake","Match":"REDACTED","Secret":"REDACTED","File":"%s"}' "$sep" "$f"; sep=","; rc=3
  fi
done
printf ']\n'; exit $rc
"#;
