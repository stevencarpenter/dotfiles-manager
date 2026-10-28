//! `packages`: the `homebrew/v1` and `mise/v1` adapters through the isolated binary (DFM-23,
//! docs/packages.md).
//!
//! Every run passes `--home <temp dir>` and an empty environment, plus `DFM_PROVIDER_PATH`
//! pointing at a directory with `brew` and `mise` look-alikes. Each look-alike logs its
//! arguments and the provider variables it saw, lists what an `installed` file names, and
//! installs by appending to that file, so no real provider runs.

mod common;

use common::{TempDir, code, text};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_dfm");
const BREW_OP: &str = "extra.atuin@package";
const MISE_OP: &str = "extra.duckdb@package";

const EXTRA: &str = r#"schema = 1
id = "extra"
platforms = ["macos", "linux"]
requires = []

[[resources]]
id = "atuin"
kind = "package"
adapter = "homebrew/v1"
[resources.value]
name = "atuin"
update_policy = "rolling"
downgrade = "forbid"

[[resources]]
id = "duckdb"
kind = "package"
adapter = "mise/v1"
[resources.value]
name = "duckdb"
update_policy = "rolling"
"#;

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
const FAKE_BREW: &str = r#"#!/bin/sh
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
/// its truncated tool column. `mise install <name>@<version>` appends; a bare name exits 1,
/// as real mise installs only the configured version for one and none is configured under
/// `MISE_NO_CONFIG=1`.
const FAKE_MISE: &str = r#"#!/bin/sh
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
    case "$2" in *@*) ;; *) echo "mise ERROR no version requested for $2" >&2; exit 1 ;; esac
    echo "${2%%@*}  2.0.0" >> "$db"; exit 0 ;;
  *) exit 64 ;;
esac
"#;

/// `dpkg-query -W -f=${Status} <name>` prints `install ok installed` and exits 0 for a name in
/// `installed-apt`, exits 1 for any other name, and exits 2 with the `broken-apt` marker.
const FAKE_DPKG: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
printf 'dpkg-query %s\n' "$*" >> "$here/../log"
[ -e "$here/../broken-apt" ] && exit 2
if grep -qx "$3" "$here/../installed-apt" 2>/dev/null; then printf 'install ok installed'; exit 0; fi
echo "dpkg-query: no packages found matching $3" >&2; exit 1
"#;

struct Fx {
    home: TempDir,
    _src: TempDir,
    root: PathBuf,
    fake: TempDir,
}

fn stderr(o: &Output) -> String {
    text(&o.stderr)
}

fn digest_of(o: &Output) -> String {
    text(&o.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("plan digest: ").map(str::to_string))
        .unwrap_or_else(|| panic!("no digest in\n{}\n{}", text(&o.stdout), stderr(o)))
}

impl Fx {
    fn new() -> Fx {
        Fx::with_home("packages-home")
    }

    /// A fixture whose HOME directory name is `label`, for paths that resemble a provider name.
    fn with_home(label: &str) -> Fx {
        let fx = Fx {
            home: TempDir::new(label),
            _src: TempDir::new("packages-src"),
            root: PathBuf::new(),
            fake: TempDir::new("packages-providers"),
        };
        let root = fx._src.path().join("root");
        let fx = Fx { root, ..fx };
        fx.put("base/bundle.toml", common::BUNDLE);
        fx.put("base/modules/core/module.toml", common::CORE);
        fx.put("base/modules/core/files/gitconfig", "[user]\n");
        fx.put("base/modules/core/files/ssh_config", "Host *\n");
        fx.put("base/modules/extra/module.toml", EXTRA);
        fx.put("profiles/t-01.toml", common::PROFILE);
        fs::create_dir_all(fx.bin()).unwrap();
        for (name, body) in [
            ("brew", FAKE_BREW),
            ("mise", FAKE_MISE),
            ("dpkg-query", FAKE_DPKG),
        ] {
            let p = fx.bin().join(name);
            fs::write(&p, body).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let p = fx.root.join("profiles/t-01.toml");
        let o = fx.run(&[
            "bind",
            "--profile",
            p.to_str().unwrap(),
            "--machine-id",
            "m5-test",
            "--root",
            fx.root.to_str().unwrap(),
        ]);
        assert_eq!(code(&o), 0, "{}", stderr(&o));
        fx
    }

    fn bin(&self) -> PathBuf {
        self.fake.path().join("bin")
    }

    fn put(&self, rel: &str, content: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    /// The installed packages of a provider, one `<name> <version>` line each. For brew they
    /// become `Cellar/<name>/<version>` kegs with an install receipt naming `homebrew/core`, as
    /// Homebrew keeps them.
    fn installed(&self, provider: &str, lines: &str) {
        fs::write(
            self.fake.path().join(format!("installed-{provider}")),
            lines,
        )
        .unwrap();
        if provider == "brew" {
            let cellar = self.fake.path().join("Cellar");
            let _ = fs::remove_dir_all(&cellar);
            fs::create_dir_all(&cellar).unwrap();
            for l in lines.lines() {
                let mut p = l.split_whitespace();
                let (n, v) = (p.next().unwrap(), p.next().unwrap());
                let keg = cellar.join(n).join(v);
                fs::create_dir_all(&keg).unwrap();
                fs::write(
                    keg.join("INSTALL_RECEIPT.json"),
                    r#"{"source":{"tap":"homebrew/core"}}"#,
                )
                .unwrap();
            }
        }
    }

    fn log(&self) -> String {
        fs::read_to_string(self.fake.path().join("log")).unwrap_or_default()
    }

    fn provider_ran(&self) -> bool {
        self.fake.path().join("log").exists()
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.env_clear()
            .env("DFM_PROVIDER_PATH", self.bin())
            .env("USER", "leak-check")
            .arg("--home")
            .arg(self.home.path())
            .args(args);
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn root_arg(&self) -> &str {
        self.root.to_str().unwrap()
    }

    fn preview(&self) -> Output {
        self.run(&["packages", "--root", self.root_arg(), "--preview"])
    }

    fn packages(&self, digest: &str) -> Output {
        self.run(&["packages", "--root", self.root_arg(), "--yes-plan", digest])
    }

    fn state(&self) -> PathBuf {
        self.home.path().join(".local/state/dotfiles-manager/state")
    }

    fn receipts(&self) -> Vec<serde_json::Value> {
        let Ok(dir) = fs::read_dir(self.state().join("receipts")) else {
            return Vec::new();
        };
        dir.map(|e| serde_json::from_slice(&fs::read(e.unwrap().path()).unwrap()).unwrap())
            .collect()
    }
}

#[test]
fn a_configuration_plan_lists_packages_out_of_scope_and_starts_no_provider() {
    let fx = Fx::new();
    let o = fx.run(&["plan", "--root", fx.root_arg()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let t = text(&o.stdout);
    assert!(t.contains("not_in_scope extra.atuin@scope"), "{t}");
    assert!(t.contains("not_in_scope extra.duckdb@scope"), "{t}");
    assert!(!fx.provider_ran(), "plan started a provider: {}", fx.log());
    // No provider has run, so the runner's working directory does not exist. Doctor creates
    // nothing, so it starts no provider and reports each package unknown.
    let o = fx.run(&["doctor", "--root", fx.root_arg()]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains("UNKNOWN E_PROBE_FAILED resource"), "{t}");
    assert!(t.contains("does not exist yet"), "{t}");
    assert!(
        !fx.provider_ran(),
        "doctor started a provider: {}",
        fx.log()
    );
    assert!(!fx.home.path().join(".cache/dotfiles-manager/run").exists());
}

/// The JSON row of `resource` in `doctor --json` run with `PATH`, and the document.
fn doctor_row(
    fx: &Fx,
    path: &std::path::Path,
    resource: &str,
) -> (serde_json::Value, serde_json::Value) {
    let o = fx
        .cmd(&["doctor", "--root", fx.root_arg(), "--json"])
        .env("PATH", path)
        .output()
        .unwrap();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let row = v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["subject"] == resource)
        .unwrap_or_else(|| panic!("no {resource} row in {v:#}"))
        .clone();
    (row, v)
}

#[test]
fn doctor_shows_provider_version_and_path_selection_and_reports_another_owner_unadopted() {
    let fx = Fx::new();
    fx.installed("brew", "atuin 18.24.1\n");
    fx.installed("mise", "");
    // A preview runs the providers once, which makes the runner's working directory.
    assert_eq!(code(&fx.preview()), 0);
    // PATH selects `atuin` through a link. Doctor reads the link and never runs its target.
    let real = fx.fake.path().join("real");
    let path = fx.fake.path().join("pathbin");
    fs::create_dir_all(&real).unwrap();
    fs::create_dir_all(&path).unwrap();
    let ran = fx.fake.path().join("atuin-ran");
    fs::write(
        real.join("atuin"),
        format!("#!/bin/sh\ntouch {}\n", ran.display()),
    )
    .unwrap();
    fs::set_permissions(real.join("atuin"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("../real/atuin", path.join("atuin")).unwrap();
    fs::remove_file(fx.fake.path().join("log")).unwrap();

    let (row, v) = doctor_row(&fx, &path, "extra.atuin");
    assert_eq!(
        (row["status"].as_str(), row["code"].as_str()),
        (Some("OK"), Some("D_OK")),
        "{row}"
    );
    let d = row["detail"].as_str().unwrap();
    assert!(
        d.contains("atuin is installed by homebrew/v1 (18.24.1)"),
        "{d}"
    );
    let link = path.join("atuin");
    assert!(
        d.contains(&format!(
            "PATH selects {} -> {}",
            link.display(),
            path.join("../real/atuin").display()
        )),
        "{d}"
    );
    assert_eq!(v["path"], path.to_str().unwrap());
    assert!(v["observed_unix"].as_u64().unwrap() > 0, "{v}");
    let (row, _) = doctor_row(&fx, &path, "extra.duckdb");
    assert_eq!(
        (row["status"].as_str(), row["code"].as_str()),
        (Some("WARN"), Some("D_PLAN_PENDING")),
        "{row}"
    );
    assert!(
        row["detail"]
            .as_str()
            .unwrap()
            .contains("PATH selects no executable named duckdb"),
        "{row}"
    );

    // An executable of the name beside the providers is another owner: a warning, not adopted.
    let other = fx.bin().join("duckdb");
    fs::write(&other, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&other, fs::Permissions::from_mode(0o755)).unwrap();
    let (row, _) = doctor_row(&fx, &path, "extra.duckdb");
    assert_eq!(
        (row["status"].as_str(), row["code"].as_str()),
        (Some("WARN"), Some("E_CONFLICT")),
        "{row}"
    );
    assert!(
        row["detail"].as_str().unwrap().contains("not adopted"),
        "{row}"
    );

    // The probes were the read-only listings; nothing was installed or executed from PATH.
    let log = fx.log();
    assert!(log.contains("brew --cellar"), "{log}");
    assert!(log.contains("mise ls --installed --json duckdb"), "{log}");
    assert!(!log.contains(" install "), "{log}");
    assert!(!ran.exists(), "doctor ran the executable PATH selects");
}

#[test]
fn missing_packages_are_installed_once_installed_ones_are_current_and_the_env_is_rebuilt() {
    let fx = Fx::new();
    fx.installed("brew", "atuin 18.24.1\n");
    fx.installed("mise", "");
    let o = fx.preview();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains("scope=packages"), "{t}");
    assert!(t.contains(&format!("current {BREW_OP}")), "{t}");
    assert!(t.contains("installed by homebrew/v1 (18.24.1)"), "{t}");
    assert!(t.contains(&format!("planned {MISE_OP}")), "{t}");
    assert!(t.contains("install duckdb with mise/v1"), "{t}");
    assert!(t.contains("not_in_scope core.git@scope"), "{t}");
    // The probes ran read-only, with the provider variables set and nothing of ours leaked.
    let log = fx.log();
    assert!(
        log.contains(
            "brew --cellar | auto=1 analytics=1 hints=1 cleanup=1 upgrade=1 dependents=1 user=none"
        ),
        "{log}"
    );
    assert!(
        log.contains("mise ls --installed --json duckdb | yes=1 auto=false noconfig=1"),
        "{log}"
    );
    assert!(!log.contains(" install "), "{log}");
    let o = fx.packages(&digest_of(&o));
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    assert!(out.contains(&format!("updated {MISE_OP}")), "{out}");
    assert!(out.contains(&format!("current {BREW_OP}")), "{out}");
    assert!(out.contains("result: ok"), "{out}");
    assert!(
        fx.log()
            .contains("mise install duckdb@latest | yes=1 auto=false noconfig=1"),
        "{}",
        fx.log()
    );
    assert_eq!(
        fs::read_to_string(fx.fake.path().join("installed-mise")).unwrap(),
        "duckdb  2.0.0\n"
    );
    // The receipt names the command and the operation, with no outputs and no fingerprints.
    let r = fx.receipts();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0]["command"], "packages");
    let ops = r[0]["operations"].as_array().unwrap();
    let op = ops
        .iter()
        .find(|o| o["resource"] == "extra.duckdb")
        .unwrap();
    assert_eq!(op["status"], "updated", "{op}");
    assert!(op["owned_outputs"].as_array().unwrap().is_empty());
    assert!(op["before"].is_null() && op["after"].is_null() && op["reload"].is_null());
    // No ledger entry: a package is not an owned file.
    let ledger: serde_json::Value =
        serde_json::from_slice(&fs::read(fx.state().join("ledger.json")).unwrap()).unwrap();
    assert!(
        ledger["resources"].as_array().unwrap().is_empty(),
        "{ledger}"
    );
    // A rerun finds both installed: current, no receipt.
    let o = fx.packages(&digest_of(&fx.preview()));
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(text(&o.stdout).contains(&format!("current {MISE_OP}")));
    assert_eq!(fx.receipts().len(), 1);
}

#[test]
fn another_owner_blocks_and_a_failed_install_is_reported_with_stderr_withheld() {
    let fx = Fx::new();
    fx.installed("brew", "");
    fx.installed("mise", "duckdb 1.5.6\n");
    // An executable named like the package in another prefix is a conflict, not an install.
    let other = fx.home.path().join(".local/bin");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("atuin"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(other.join("atuin"), fs::Permissions::from_mode(0o755)).unwrap();
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert!(stderr(&o).contains("another owner"), "{}", stderr(&o));
    assert!(
        text(&o.stdout).contains("blocked extra.atuin@package"),
        "{}",
        text(&o.stdout)
    );
    fs::remove_file(other.join("atuin")).unwrap();
    // A provider that cannot install: failed, exit 1, stderr withheld, nothing else changed.
    fs::write(fx.fake.path().join("fail"), "").unwrap();
    let o = fx.preview();
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let o = fx.packages(&digest_of(&o));
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    assert!(out.contains(&format!("failed {BREW_OP}")), "{out}");
    assert!(out.contains("withheld"), "{out}");
    assert!(!out.contains("PLANTED-STDERR") && !stderr(&o).contains("PLANTED-STDERR"));
    assert!(out.contains("result: failed"), "{out}");
    let r = fx.receipts();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0]["result"]["outcome"], "failed");
    let journal = fs::read_to_string(fx.state().join("journal.jsonl")).unwrap();
    assert!(journal.contains("\"op\":\"install\""), "{journal}");
    assert!(journal.contains("\"failed\""), "{journal}");
    // Without the failure the next run installs it.
    fs::remove_file(fx.fake.path().join("fail")).unwrap();
    let o = fx.packages(&digest_of(&fx.preview()));
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(text(&o.stdout).contains(&format!("updated {BREW_OP}")));
}

#[test]
fn qualified_names_are_probed_by_their_short_name_and_installed_in_full() {
    let fx = Fx::new();
    fx.put(
        "base/modules/extra/module.toml",
        &EXTRA
            .replace("name = \"atuin\"", "name = \"homebrew/core/atuin\"")
            .replace("name = \"duckdb\"", "name = \"duckdb@2\""),
    );
    fx.installed("brew", "atuin 18.24.1\n");
    fx.installed("mise", "duckdb 1.5.6\n");
    let o = fx.preview();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains(&format!("current {BREW_OP}")), "{t}");
    assert!(t.contains("(18.24.1)"), "{t}");
    // 1.5.6 is installed, but not a 2.x: the declared version is missing.
    assert!(t.contains(&format!("planned {MISE_OP}")), "{t}");
    let o = fx.packages(&digest_of(&o));
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let log = fx.log();
    assert!(!log.contains("brew list"), "{log}");
    assert!(log.contains("mise ls --installed --json duckdb |"), "{log}");
    assert!(log.contains("mise install duckdb@2 |"), "{log}");
    assert!(text(&o.stdout).contains(&format!("updated {MISE_OP}")));
    // A rack is keyed by the short name only: for a tap-qualified name the keg's receipt must
    // name that tap. A keg from another tap is another owner; no receipt fails closed. A bare
    // name accepts any tap.
    let receipt = fx
        .fake
        .path()
        .join("Cellar/atuin/18.24.1/INSTALL_RECEIPT.json");
    fs::write(&receipt, r#"{"source":{"tap":"someone/tap"}}"#).unwrap();
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("installed from the tap someone/tap, not from homebrew/core"),
        "{}",
        stderr(&o)
    );
    fs::remove_file(&receipt).unwrap();
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PROBE_FAILED"), "{}", stderr(&o));
    fs::write(&receipt, r#"{"source":{"tap":"someone/tap"}}"#).unwrap();
    fx.put("base/modules/extra/module.toml", EXTRA);
    let o = fx.preview();
    assert!(
        text(&o.stdout).contains(&format!("current {BREW_OP}")),
        "{}\n{}",
        stderr(&o),
        text(&o.stdout)
    );
    // A prefix matches whole components: 10.x is not a 1.x. A selector mise resolves online
    // is a manifest error, not a probe.
    fx.put(
        "base/modules/extra/module.toml",
        &EXTRA.replace("name = \"duckdb\"", "name = \"duckdb@1\""),
    );
    fx.installed("mise", "duckdb 10.0.0\n");
    let o = fx.preview();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains(&format!("planned {MISE_OP}")),
        "{}",
        text(&o.stdout)
    );
    fx.put(
        "base/modules/extra/module.toml",
        &EXTRA.replace("name = \"duckdb\"", "name = \"duckdb@latest\""),
    );
    let lines = fx.log().lines().count();
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_MANIFEST_INVALID"), "{}", stderr(&o));
    assert!(!fx.log().contains("duckdb@latest"));
    assert!(
        !fx.log()[..]
            .lines()
            .skip(lines)
            .any(|l| l.starts_with("mise ")),
        "{}",
        fx.log()
    );
    // A scoped npm tool keeps its `@`: it is listed in full and current when installed.
    fx.put(
        "base/modules/extra/module.toml",
        &EXTRA.replace("name = \"duckdb\"", "name = \"npm:@scope/acp\""),
    );
    fx.installed("mise", "npm:@scope/acp  0.79.0\n");
    let o = fx.preview();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains(&format!("current {MISE_OP}")),
        "{}",
        text(&o.stdout)
    );
    assert!(
        fx.log()
            .contains("mise ls --installed --json npm:@scope/acp |"),
        "{}",
        fx.log()
    );
}

#[test]
fn the_owner_check_survives_a_home_that_resembles_a_provider_and_a_noop_install_fails() {
    // The HOME path holds "mise" and "homebrew": the owner check must still look in it.
    let fx = Fx::with_home("promise-homebrew-home");
    fx.installed("brew", "");
    fx.installed("mise", "duckdb 1\n");
    let other = fx.home.path().join(".local/bin");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("atuin"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(other.join("atuin"), fs::Permissions::from_mode(0o755)).unwrap();
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("another owner"), "{}", stderr(&o));
    fs::remove_file(other.join("atuin")).unwrap();
    // An install that exits 0 without installing fails its postcondition.
    fs::write(fx.fake.path().join("noop"), "").unwrap();
    let o = fx.packages(&digest_of(&fx.preview()));
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    assert!(out.contains(&format!("failed {BREW_OP}")), "{out}");
    assert!(out.contains("postcondition failed"), "{out}");
    // The install runs with the same variables: no upgrade of the formula or its dependents.
    assert!(
        fx.log().contains(
            "brew install homebrew/core/atuin | auto=1 analytics=1 hints=1 cleanup=1 upgrade=1 dependents=1 user=none"
        ),
        "{}",
        fx.log()
    );
    // A bad name is a manifest error, not a probe failure.
    fx.put(
        "base/modules/extra/module.toml",
        &EXTRA.replace("name = \"atuin\"", "name = \"a b\""),
    );
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}", stderr(&o));
    assert!(stderr(&o).contains("E_MANIFEST_INVALID"), "{}", stderr(&o));
}

#[test]
fn brew_dependencies_are_planned_when_missing_and_block_when_an_install_would_upgrade_them() {
    let fx = Fx::new();
    fx.installed("brew", "libpng 1.6.0\n");
    fx.installed("mise", "duckdb 1\n");
    fs::write(fx.fake.path().join("deps-atuin"), "libpng\naom\n").unwrap();
    // aom is missing: it is part of the planned effect and of the probe precondition.
    let o = fx.preview();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains(&format!("planned {BREW_OP}")), "{t}");
    assert!(t.contains("and its missing dependencies aom"), "{t}");
    assert!(!t.contains("dependencies libpng"), "{t}");
    let log = fx.log();
    assert!(log.contains("brew formulae |"), "{log}");
    assert!(log.contains("brew deps homebrew/core/atuin |"), "{log}");
    assert!(!log.contains("brew list"), "{log}");
    assert!(
        log.contains("brew outdated --json=v2 homebrew/core/libpng |"),
        "{log}"
    );
    assert!(!log.contains("brew install"), "{log}");
    // A dependency that appears after the preview makes the approved plan stale.
    let d = digest_of(&o);
    fs::write(fx.fake.path().join("deps-atuin"), "libpng\naom\nx265\n").unwrap();
    let o = fx.packages(&d);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    assert!(!fx.log().contains("brew install"), "{}", fx.log());
    // A missing dependency without a bottle for this machine would be built from source, with
    // build-time dependencies the listing does not name: the plan is blocked.
    assert!(
        fx.log().contains("brew ruby -e ")
            && fx.log().contains("homebrew/core/atuin homebrew/core/aom |"),
        "{}",
        fx.log()
    );
    fs::write(fx.fake.path().join("deps-atuin"), "libpng\naom\n").unwrap();
    fs::write(fx.fake.path().join("nobottle-brew"), "aom\n").unwrap();
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        stderr(&o).contains("E_PLATFORM_UNSUPPORTED"),
        "{}",
        stderr(&o)
    );
    assert!(
        stderr(&o).contains("aom has no prebuilt binary"),
        "{}",
        stderr(&o)
    );
    fs::remove_file(fx.fake.path().join("nobottle-brew")).unwrap();
    // An installed dependency the install would upgrade blocks the plan.
    fs::write(fx.fake.path().join("outdated-brew"), "libpng\n").unwrap();
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_CONFLICT"), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("would upgrade the installed dependencies libpng"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn a_package_that_is_not_official_is_blocked_before_any_tap_code_is_read() {
    let fx = Fx::new();
    fx.installed("brew", "");
    fx.installed("mise", "duckdb 1\n");
    // A tap-qualified name, then a bare name Homebrew does not list as official.
    for (name, list) in [("someone/tap/atuin", None), ("atuin", Some("duckdb\n"))] {
        fx.put(
            "base/modules/extra/module.toml",
            &EXTRA.replace("name = \"atuin\"", &format!("name = \"{name}\"")),
        );
        if let Some(l) = list {
            fs::write(fx.fake.path().join("official-brew"), l).unwrap();
        }
        let o = fx.preview();
        assert_eq!(code(&o), 3, "{name}: {}\n{}", stderr(&o), text(&o.stdout));
        assert!(stderr(&o).contains("E_SOURCE_UNTRUSTED"), "{}", stderr(&o));
        assert!(
            stderr(&o).contains("not an official Homebrew"),
            "{}",
            stderr(&o)
        );
    }
    let log = fx.log();
    for call in ["brew deps", "brew info", "brew outdated", "brew install"] {
        assert!(!log.contains(call), "{call} ran: {log}");
    }
}

#[test]
fn an_official_cask_is_probed_and_installed_as_a_cask() {
    let fx = Fx::new();
    fx.installed("brew", "");
    fx.installed("mise", "duckdb 1\n");
    // The short name is a cask, not a formula: every call names it with --cask, so brew never
    // resolves it to a formula of that name from a tap.
    fx.put(
        "base/modules/extra/module.toml",
        &EXTRA.replace("name = \"atuin\"", "name = \"zed\""),
    );
    fs::write(fx.fake.path().join("official-brew"), "duckdb\n").unwrap();
    fs::write(fx.fake.path().join("official-cask"), "zed\n").unwrap();
    let o = fx.preview();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let o = fx.packages(&digest_of(&o));
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let log = fx.log();
    assert!(log.contains("brew deps --cask zed |"), "{log}");
    assert!(log.contains("brew install --cask zed |"), "{log}");
    assert!(!log.contains("brew info"), "{log}");
    assert!(!log.contains("homebrew/core/zed"), "{log}");
    // The installed cask is found in the Caskroom: the rerun is current, nothing is installed.
    let lines = fx.log().lines().count();
    let o = fx.preview();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains(&format!("current {BREW_OP}")),
        "{}",
        text(&o.stdout)
    );
    // An installed cask from a tap is current too, and nothing that loads it runs: the probe
    // reads the Caskroom only.
    fx.put(
        "base/modules/extra/module.toml",
        &EXTRA.replace("name = \"atuin\"", "name = \"tapcask\""),
    );
    fs::create_dir_all(fx.fake.path().join("Caskroom/tapcask/2.0")).unwrap();
    let o = fx.preview();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(text(&o.stdout).contains("(2.0)"), "{}", text(&o.stdout));
    let later: Vec<String> = fx.log().lines().skip(lines).map(String::from).collect();
    assert!(
        later
            .iter()
            .filter(|l| l.starts_with("brew "))
            .all(|l| l.starts_with("brew --cellar") || l.starts_with("brew --caskroom")),
        "{later:?}"
    );
}

#[test]
fn two_formulae_that_share_a_missing_dependency_are_both_installed() {
    let fx = Fx::new();
    // Both packages are Homebrew formulae that need aom. The first install adds it, so the
    // second one's probe under the lock no longer lists it: that is not a stale plan.
    fx.put(
        "base/modules/extra/module.toml",
        &EXTRA.replace("adapter = \"mise/v1\"", "adapter = \"homebrew/v1\""),
    );
    fx.installed("brew", "");
    fx.installed("mise", "");
    for p in ["atuin", "duckdb"] {
        fs::write(fx.fake.path().join(format!("deps-{p}")), "aom\n").unwrap();
    }
    let o = fx.preview();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert_eq!(
        t.matches("and its missing dependencies aom").count(),
        2,
        "{t}"
    );
    let o = fx.packages(&digest_of(&o));
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    assert!(out.contains(&format!("updated {BREW_OP}")), "{out}");
    assert!(out.contains(&format!("updated {MISE_OP}")), "{out}");
    assert!(fx.fake.path().join("Cellar/aom/1.0.0").is_dir());
}

#[test]
fn an_apt_package_is_probed_with_dpkg_query_and_never_installed() {
    let fx = Fx::new();
    fx.installed("mise", "duckdb 1\n");
    fx.put(
        "base/modules/extra/module.toml",
        &EXTRA.replace("adapter = \"homebrew/v1\"", "adapter = \"apt/v1\""),
    );
    // Installed: current, from the status dpkg-query reports.
    fs::write(fx.fake.path().join("installed-apt"), "atuin\n").unwrap();
    let o = fx.preview();
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        text(&o.stdout).contains(&format!("current {BREW_OP}")),
        "{}",
        text(&o.stdout)
    );
    assert!(
        fx.log().contains("dpkg-query -W -f=${Status} atuin"),
        "{}",
        fx.log()
    );
    // Missing: installing needs root, so the plan is blocked with the command to run by hand.
    fs::write(fx.fake.path().join("installed-apt"), "").unwrap();
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        stderr(&o).contains("E_PRIVILEGE_UNSUPPORTED"),
        "{}",
        stderr(&o)
    );
    assert!(
        stderr(&o).contains("sudo apt-get install atuin"),
        "{}",
        stderr(&o)
    );
    // Any other exit is a probe that could not answer.
    fs::write(fx.fake.path().join("broken-apt"), "").unwrap();
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PROBE_FAILED"), "{}", stderr(&o));
    assert!(stderr(&o).contains("dpkg-query exited 2"), "{}", stderr(&o));
    assert!(!fx.log().contains("apt-get"), "{}", fx.log());
}

#[test]
fn an_installed_package_with_an_unresolved_install_stays_blocked_until_resolved() {
    for (provider, op_id) in [("brew", BREW_OP), ("mise", MISE_OP)] {
        let fx = Fx::new();
        fx.installed("brew", if provider == "brew" { "" } else { "atuin 1\n" });
        fx.installed("mise", if provider == "mise" { "" } else { "duckdb 1\n" });
        let installs = || fx.log().lines().filter(|l| l.contains(" install ")).count();
        // A failure before the effect is journaled as failed, and the provider never ran.
        let o = fx
            .cmd(&[
                "packages",
                "--root",
                fx.root_arg(),
                "--yes-plan",
                &digest_of(&fx.preview()),
            ])
            .env("DFM_FAILPOINT", "apply-before-effect:error")
            .output()
            .unwrap();
        assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
        let all = format!("{}{}", stderr(&o), text(&o.stdout));
        assert!(
            all.contains("injected failure at apply-before-effect"),
            "{all}"
        );
        assert_eq!(installs(), 0);
        let d = digest_of(&fx.preview());
        // The provider finished, but the manager never recorded the outcome.
        let crash = fx
            .cmd(&["packages", "--root", fx.root_arg(), "--yes-plan", &d])
            .env("DFM_FAILPOINT", "apply-after-effect:abort")
            .output()
            .unwrap();
        assert!(!crash.status.success());
        let journal = fx.state().join("journal.jsonl");
        let intent: serde_json::Value = serde_json::from_str(
            fs::read_to_string(&journal)
                .unwrap()
                .lines()
                .last()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(intent["phase"], "intent");
        let id = intent["op_id"].as_str().unwrap();
        assert_eq!(installs(), 1);
        let d = digest_of(&fx.preview());
        for _ in 0..2 {
            let o = fx.packages(&d);
            assert_ne!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
            assert!(stderr(&o).contains("E_OP_INTERRUPTED"), "{}", stderr(&o));
            let out = text(&o.stdout);
            assert!(out.contains(&format!("blocked {op_id}")), "{out}");
            assert!(!out.contains("result: ok"), "{out}");
            assert!(stderr(&o).contains(id), "{}", stderr(&o));
            assert_eq!(installs(), 1, "the unresolved install must not run again");
        }
        // Every operation is current, yet the open entry still needs the approved run.
        let o = fx.run(&["packages", "--root", fx.root_arg()]);
        assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
        assert!(stderr(&o).contains("E_APPROVAL_REQUIRED"), "{}", stderr(&o));
        let o = fx.run(&["state", "recover"]);
        assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
        assert!(text(&o.stdout).contains(id));
        let o = fx.run(&["state", "resolve", id, "--as", "failed"]);
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
        let o = fx.packages(&digest_of(&fx.preview()));
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
        assert!(text(&o.stdout).contains(&format!("current {op_id}")));
        assert_eq!(installs(), 1, "resolution must not reinstall the package");
        let o = fx.run(&["packages", "--root", fx.root_arg()]);
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
        assert!(text(&o.stdout).contains("nothing to change: no approval needed"));
    }
}

#[test]
fn a_receipt_write_failure_after_the_install_does_not_install_again() {
    let fx = Fx::new();
    fx.installed("brew", "");
    fx.installed("mise", "duckdb 1\n");
    let installs = || fx.log().lines().filter(|l| l.contains(" install ")).count();
    let o = fx
        .cmd(&[
            "packages",
            "--root",
            fx.root_arg(),
            "--yes-plan",
            &digest_of(&fx.preview()),
        ])
        .env("DFM_FAILPOINT", "receipt-after-tmp:error")
        .output()
        .unwrap();
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    let all = format!("{}{}", stderr(&o), text(&o.stdout));
    assert!(
        all.contains("injected failure at receipt-after-tmp"),
        "{all}"
    );
    assert_eq!(installs(), 1);
    let o = fx.packages(&digest_of(&fx.preview()));
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(text(&o.stdout).contains(&format!("current {BREW_OP}")));
    assert_eq!(
        installs(),
        1,
        "a lost receipt must not reinstall the package"
    );
}

#[test]
fn a_stop_that_cuts_off_the_postcondition_is_reported_as_the_stop() {
    let fx = Fx::new();
    fx.installed("brew", "");
    fx.installed("mise", "duckdb 1\n");
    let d = digest_of(&fx.preview());
    // The stop request arrives right after the install, before the postcondition probe.
    let o = fx
        .cmd(&["packages", "--root", fx.root_arg(), "--yes-plan", &d])
        .env("DFM_FAILPOINT", "apply-after-effect:stop")
        .output()
        .unwrap();
    assert_eq!(code(&o), 143, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    assert!(out.contains(&format!("failed {BREW_OP}")), "{out}");
    assert!(
        out.contains("stopped before the postcondition was checked"),
        "{out}"
    );
    assert!(!out.contains("postcondition failed"), "{out}");
}

#[test]
fn a_stop_while_a_descendant_holds_the_output_ends_the_run_without_the_grace_wait() {
    let fx = Fx::new();
    fx.installed("brew", "");
    fx.installed("mise", "duckdb 1\n");
    // The provider exits at once but leaves a process holding its output, and the manager gets
    // SIGTERM meanwhile: the holder is ended now, not after the 10 s grace period.
    fs::write(fx.fake.path().join("linger"), "").unwrap();
    let t = std::time::Instant::now();
    let o = fx.preview();
    assert_eq!(code(&o), 143, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        t.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        t.elapsed()
    );
}

#[test]
fn a_stop_request_during_a_probe_ends_the_probing_and_exits_143() {
    let fx = Fx::new();
    fx.installed("brew", "atuin 1\n");
    fx.installed("mise", "duckdb 1\n");
    // The brew look-alike (the first probe) sends SIGTERM to the manager. The mise probe must
    // not start, and the run exits as terminated, before any write.
    fs::write(fx.fake.path().join("stop"), "").unwrap();
    let o = fx.preview();
    assert_eq!(code(&o), 143, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_TERMINATED"), "{}", stderr(&o));
    let log = fx.log();
    assert!(log.contains("brew --cellar"), "{log}");
    assert!(!log.contains("mise "), "{log}");
    assert!(fx.receipts().is_empty());
}

#[test]
fn a_provider_that_cannot_answer_blocks_and_the_flags_of_apply_are_refused() {
    let fx = Fx::new();
    fx.installed("brew", "atuin 1\n");
    fx.put(
        "base/modules/extra/module.toml",
        &EXTRA.replace("name = \"duckdb\"", "name = \"zzz-unknown\""),
    );
    let o = fx.preview();
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PROBE_FAILED"), "{}", stderr(&o));
    for flags in [
        &["--allow-disruptive", "x"][..],
        &["--allow-adopt", "x"][..],
    ] {
        let mut a = vec!["packages", "--root", fx.root_arg()];
        a.extend_from_slice(flags);
        assert_eq!(code(&fx.run(&a)), 2);
    }
}
