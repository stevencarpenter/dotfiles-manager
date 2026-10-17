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

/// `brew list --versions <name>` answers from `installed-brew`; `brew install <name>` appends.
const FAKE_BREW: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
printf 'brew %s | auto=%s analytics=%s user=%s\n' "$*" "${HOMEBREW_NO_AUTO_UPDATE:-unset}" "${HOMEBREW_NO_ANALYTICS:-unset}" "${USER:-none}" >> "$here/../log"
db="$here/../installed-brew"
case "$1" in
  list)
    name=$3
    v=$(grep "^$name " "$db" 2>/dev/null | head -1)
    if [ -n "$v" ]; then echo "$v"; exit 0; fi
    exit 1 ;;
  install)
    [ -e "$here/../fail" ] && { echo "Error: PLANTED-STDERR" >&2; exit 1; }
    echo "$2 1.0.0" >> "$db"; echo "==> Installing $2"; exit 0 ;;
  *) exit 64 ;;
esac
"#;

/// `mise ls --installed <name>` answers from `installed-mise`; `mise install <name>` appends.
const FAKE_MISE: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
printf 'mise %s | yes=%s auto=%s\n' "$*" "${MISE_YES:-unset}" "${MISE_NOT_FOUND_AUTO_INSTALL:-unset}" >> "$here/../log"
db="$here/../installed-mise"
case "$1" in
  ls)
    name=$3
    [ "$name" = "zzz-unknown" ] && { echo "mise ERROR $name not found in mise tool registry" >&2; exit 1; }
    grep "^$name " "$db" 2>/dev/null
    exit 0 ;;
  install)
    echo "$2  2.0.0" >> "$db"; exit 0 ;;
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
        let fx = Fx {
            home: TempDir::new("packages-home"),
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
    assert_eq!(fx.log(), "", "a provider was started by plan or doctor");
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
        log.contains("brew list --versions atuin | auto=1 analytics=1 user=none"),
        "{log}"
    );
    assert!(
        log.contains("mise ls --installed duckdb | yes=1 auto=false"),
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
        fx.log().contains("mise install duckdb | yes=1 auto=false"),
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
