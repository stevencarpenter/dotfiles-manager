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

const BIN: &str = env!("CARGO_BIN_EXE_dotfiles-manager");
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

/// `brew list --versions <name>...` answers from `installed-brew` and prints the short names, as
/// brew does for a tap-qualified name, exiting 1 if any is absent; `brew deps <name>` answers
/// from `deps-<name>`; `brew outdated --json=v2 <name>...` reports the names listed in
/// `outdated-brew` and exits 1 when there is one; `brew info --json=v2 <name>...` reports a
/// bottle for every name except those in `nobottle-brew`; `brew install <name>` appends the
/// name and its missing dependencies, or does nothing with the `noop` marker, or fails with the
/// `fail` marker.
const FAKE_BREW: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
printf 'brew %s | auto=%s analytics=%s hints=%s cleanup=%s upgrade=%s dependents=%s user=%s\n' "$*" "${HOMEBREW_NO_AUTO_UPDATE:-unset}" "${HOMEBREW_NO_ANALYTICS:-unset}" "${HOMEBREW_NO_ENV_HINTS:-unset}" "${HOMEBREW_NO_INSTALL_CLEANUP:-unset}" "${HOMEBREW_NO_INSTALL_UPGRADE:-unset}" "${HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK:-unset}" "${USER:-none}" >> "$here/../log"
db="$here/../installed-brew"
case "$1" in
  list)
    [ -e "$here/../stop" ] && kill -TERM $PPID
    shift 2; rc=0
    for n in "$@"; do
      v=$(grep "^${n##*/} " "$db" 2>/dev/null | head -1)
      if [ -n "$v" ]; then echo "$v"; else rc=1; fi
    done
    exit $rc ;;
  deps)
    cat "$here/../deps-${2##*/}" 2>/dev/null; exit 0 ;;
  outdated)
    shift 2; rc=0; sep=""; printf '{"formulae":['
    for n in "$@"; do
      if grep -qx "$n" "$here/../outdated-brew" 2>/dev/null; then
        printf '%s{"name":"%s"}' "$sep" "$n"; sep=","; rc=1
      fi
    done
    printf '],"casks":[]}\n'; exit $rc ;;
  info)
    shift 2; sep=""; printf '{"formulae":['
    for n in "$@"; do
      if grep -qx "$n" "$here/../nobottle-brew" 2>/dev/null; then b='{}'; else b='{"stable":{"files":{"arm64_test":{}}}}'; fi
      printf '%s{"name":"%s","bottle":%s}' "$sep" "$n" "$b"; sep=","
    done
    printf '],"casks":[]}\n'; exit 0 ;;
  install)
    [ -e "$here/../fail" ] && { echo "Error: PLANTED-STDERR" >&2; exit 1; }
    [ -e "$here/../noop" ] && exit 0
    for d in $(cat "$here/../deps-${2##*/}" 2>/dev/null); do
      grep -q "^$d " "$db" 2>/dev/null || echo "$d 1.0.0" >> "$db"
    done
    echo "${2##*/} 1.0.0" >> "$db"; echo "==> Installing $2"; exit 0 ;;
  *) exit 64 ;;
esac
"#;

/// `mise ls --installed --json <name>` answers a JSON array from `installed-mise` (one
/// `<name> <version>` line per installed version); the table form is refused, as a stand-in for
/// its truncated tool column. `mise install <name>` appends.
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
    echo "${2%%@*}  2.0.0" >> "$db"; exit 0 ;;
  *) exit 64 ;;
esac
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
        for (name, body) in [("brew", FAKE_BREW), ("mise", FAKE_MISE)] {
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

    fn installed(&self, provider: &str, lines: &str) {
        fs::write(
            self.fake.path().join(format!("installed-{provider}")),
            lines,
        )
        .unwrap();
    }

    fn log(&self) -> String {
        fs::read_to_string(self.fake.path().join("log")).unwrap_or_default()
    }

    fn provider_ran(&self) -> bool {
        self.fake.path().join("log").exists()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .env_clear()
            .env("DFM_PROVIDER_PATH", self.bin())
            .env("USER", "leak-check")
            .arg("--home")
            .arg(self.home.path())
            .args(args)
            .output()
            .unwrap()
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
    let o = fx.run(&["doctor", "--root", fx.root_arg()]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(
        !fx.provider_ran(),
        "a provider was started by plan or doctor: {}",
        fx.log()
    );
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
            "brew list --versions atuin | auto=1 analytics=1 hints=1 cleanup=1 upgrade=1 dependents=1 user=none"
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
            .contains("mise install duckdb | yes=1 auto=false noconfig=unset"),
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
    assert!(
        log.contains("brew list --versions homebrew/core/atuin"),
        "{log}"
    );
    assert!(log.contains("mise ls --installed --json duckdb |"), "{log}");
    assert!(log.contains("mise install duckdb@2 |"), "{log}");
    assert!(text(&o.stdout).contains(&format!("updated {MISE_OP}")));
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
            "brew install atuin | auto=1 analytics=1 hints=1 cleanup=1 upgrade=1 dependents=1 user=none"
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
    assert!(log.contains("brew deps atuin |"), "{log}");
    assert!(log.contains("brew list --versions libpng aom |"), "{log}");
    assert!(log.contains("brew outdated --json=v2 libpng |"), "{log}");
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
        fx.log().contains("brew info --json=v2 atuin aom |"),
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
    let db = fs::read_to_string(fx.fake.path().join("installed-brew")).unwrap();
    assert_eq!(db.matches("aom ").count(), 1, "{db}");
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
    assert!(log.contains("brew list"), "{log}");
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
