//! `packages --update`: the rolling update of `mise/v1` and `uv-tool/v1` tools through the
//! isolated binary (DFM-24, docs/toolchains.md).
//!
//! Every run passes `--home <temp dir>` and an empty environment, plus `DFM_PROVIDER_PATH`
//! pointing at a directory with `mise`, `uv` and `brew` look-alikes. Each look-alike logs its
//! arguments and the provider variables it saw, lists what an `installed-<provider>` file
//! names, and changes that file only on an install or upgrade, so no real provider runs and
//! nothing reaches the network.

mod common;

use common::{TempDir, code, text};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_dotfiles-manager");
const DUCKDB: &str = "extra.duckdb@package";
const GO: &str = "extra.go@package";
const JAVA: &str = "extra.java@package";
const NUV: &str = "extra.nuv@package";

/// Two rolling mise tools (one a version prefix), one held mise tool and one rolling uv tool.
const EXTRA: &str = r#"schema = 1
id = "extra"
platforms = ["macos", "linux"]
requires = []

[[resources]]
id = "duckdb"
kind = "package"
adapter = "mise/v1"
[resources.value]
name = "duckdb"
update_policy = "rolling"

[[resources]]
id = "go"
kind = "package"
adapter = "mise/v1"
[resources.value]
name = "go@1.27"
update_policy = "rolling"

[[resources]]
id = "java"
kind = "package"
adapter = "mise/v1"
[resources.value]
name = "java"
update_policy = "hold"

[[resources]]
id = "nuv"
kind = "package"
adapter = "uv-tool/v1"
[resources.value]
name = "nuv"
update_policy = "rolling"
"#;

/// `mise ls --installed --json <tool>` answers from `installed-mise` (one `<tool> <version>`
/// line per installed version). `mise install <tool>[@prefix]` adds the version `latest-mise`
/// names for the tool (`<prefix>.0` when the prefix does not match it, `2.0.0` when there is
/// none) unless that version is listed, as mise does; it never removes one. The `fail` marker
/// makes an install fail. Any other subcommand (`prune`, `uninstall`, `upgrade`, `latest`)
/// exits 64, so a call to one fails its run.
const FAKE_MISE: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
printf 'mise %s | yes=%s auto=%s noconfig=%s\n' "$*" "${MISE_YES:-unset}" "${MISE_NOT_FOUND_AUTO_INSTALL:-unset}" "${MISE_NO_CONFIG:-unset}" >> "$here/../log"
db="$here/../installed-mise"
case "$1" in
  ls)
    [ "$2 $3" = "--installed --json" ] || exit 64
    name=$4
    sep=""; printf '['
    grep "^$name " "$db" 2>/dev/null | while read -r _ ver _; do
      printf '%s{"version":"%s","installed":true}' "$sep" "$ver"; sep=","
    done
    printf ']\n'
    exit 0 ;;
  install)
    [ -e "$here/../fail" ] && { echo "mise ERROR PLANTED" >&2; exit 1; }
    tool=${2%%@*}; want=${2#*@}; [ "$want" = "$2" ] && want=""
    new=$(grep "^$tool " "$here/../latest-mise" 2>/dev/null | head -1 | cut -d' ' -f2)
    [ -z "$new" ] && new=2.0.0
    case "$want" in "") ;; *) case "$new" in "$want".*) ;; *) new="$want.0" ;; esac ;; esac
    grep -qx "$tool $new" "$db" 2>/dev/null || echo "$tool $new" >> "$db"
    exit 0 ;;
  *) exit 64 ;;
esac
"#;

/// `uv tool list` prints `<name> v<version>` and `- <name>` per line of `installed-uv`, or
/// `No tools installed`. `uv tool upgrade <name>` replaces the version with the one
/// `latest-uv` names (`2.0.0` when there is none); `uv tool install <name>` adds `1.0.0`. The
/// `nopython` marker makes a change fail as uv does when no interpreter is found and downloads
/// are off.
const FAKE_UV: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
printf 'uv %s | noconfig=%s offline=%s downloads=%s progress=%s\n' "$*" "${UV_NO_CONFIG:-unset}" "${UV_OFFLINE:-unset}" "${UV_PYTHON_DOWNLOADS:-unset}" "${UV_NO_PROGRESS:-unset}" >> "$here/../log"
db="$here/../installed-uv"
[ "$1" = tool ] || exit 64
case "$2" in
  list)
    if [ -s "$db" ]; then
      while read -r n v; do printf '%s v%s\n- %s\n' "$n" "$v" "$n"; done < "$db"
    else
      echo "No tools installed"
    fi
    exit 0 ;;
  install|upgrade)
    [ -e "$here/../nopython" ] && { echo "error: No interpreter found" >&2; exit 2; }
    name=$3
    new=$(grep "^$name " "$here/../latest-uv" 2>/dev/null | head -1 | cut -d' ' -f2)
    [ -z "$new" ] && new=2.0.0
    [ "$2" = install ] && new=1.0.0
    grep -v "^$name " "$db" > "$db.next" 2>/dev/null; echo "$name $new" >> "$db.next"; mv "$db.next" "$db"
    exit 0 ;;
  *) exit 64 ;;
esac
"#;

/// `brew --cellar` and `brew --caskroom` print the fixture's `Cellar` and `Caskroom`; nothing
/// else is answered. A homebrew/v1 package under `--update` is probed and then held.
const FAKE_BREW: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin:$PATH
here=$(dirname "$0")
printf 'brew %s\n' "$*" >> "$here/../log"
case "$1" in
  --cellar) echo "$(cd "$here/.." && pwd)/Cellar"; exit 0 ;;
  --caskroom) echo "$(cd "$here/.." && pwd)/Caskroom"; exit 0 ;;
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
            home: TempDir::new("toolchains-home"),
            _src: TempDir::new("toolchains-src"),
            root: PathBuf::new(),
            fake: TempDir::new("toolchains-providers"),
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
        for (name, body) in [("mise", FAKE_MISE), ("uv", FAKE_UV), ("brew", FAKE_BREW)] {
            let p = fx.bin().join(name);
            fs::write(&p, body).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        // The usual state: every declared tool installed, a newer duckdb and go available.
        fx.installed(
            "mise",
            "duckdb 1.5.5\nduckdb 1.5.6\ngo 1.27.1\njava 21.0.2\n",
        );
        fx.installed("uv", "nuv 0.5.2\n");
        fx.marker("latest-mise", "duckdb 1.6.0\ngo 1.27.2\njava 22.0.0\n");
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

    fn marker(&self, name: &str, content: &str) {
        fs::write(self.fake.path().join(name), content).unwrap();
    }

    /// The installed tools of a provider, one `<name> <version>` line each.
    fn installed(&self, provider: &str, lines: &str) {
        self.marker(&format!("installed-{provider}"), lines);
    }

    fn listed(&self, provider: &str) -> String {
        fs::read_to_string(self.fake.path().join(format!("installed-{provider}")))
            .unwrap_or_default()
    }

    fn log(&self) -> String {
        fs::read_to_string(self.fake.path().join("log")).unwrap_or_default()
    }

    /// The provider commands that changed something, in order.
    fn changes(&self) -> Vec<String> {
        self.log()
            .lines()
            .filter(|l| l.contains(" install ") || l.contains(" upgrade "))
            .map(|l| l.split(" | ").next().unwrap_or(l).to_string())
            .collect()
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.env_clear()
            .env("DFM_PROVIDER_PATH", self.bin())
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

    fn preview(&self, update: bool) -> Output {
        let mut args = vec!["packages", "--root", self.root_arg(), "--preview"];
        if update {
            args.push("--update");
        }
        self.run(&args)
    }

    fn packages(&self, update: bool, digest: &str) -> Output {
        let mut args = vec!["packages", "--root", self.root_arg(), "--yes-plan", digest];
        if update {
            args.push("--update");
        }
        self.run(&args)
    }

    /// Plans and applies one run; the digest comes from the preview of the same mode.
    fn apply(&self, update: bool) -> Output {
        let o = self.preview(update);
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
        self.packages(update, &digest_of(&o))
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

    /// An executable of `name` another owner put in `~/.local/bin`.
    fn other_copy(&self, name: &str) -> PathBuf {
        let dir = self.home.path().join(".local/bin");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }
}

#[test]
fn a_rolling_update_upgrades_mise_and_uv_tools_keeps_every_installed_version_and_holds_a_held_one()
{
    let fx = Fx::new();
    let o = fx.preview(true);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains(&format!("planned {DUCKDB}")), "{t}");
    assert!(
        t.contains("upgrade duckdb with mise/v1 to the newest version the provider resolves at apply time; the installed 1.5.5 1.5.6 stay"),
        "{t}"
    );
    assert!(t.contains(&format!("planned {GO}")), "{t}");
    assert!(t.contains(&format!("planned {NUV}")), "{t}");
    assert!(t.contains(&format!("current {JAVA}")), "{t}");
    assert!(
        t.contains(
            "java is installed by mise/v1 and held (21.0.2, update_policy = hold); not upgraded"
        ),
        "{t}"
    );
    assert!(t.contains("not_in_scope core.git@scope"), "{t}");
    // The preview ran read-only listings only, with the provider variables that keep them so.
    let log = fx.log();
    assert!(
        log.contains("mise ls --installed --json duckdb | yes=1 auto=false noconfig=1"),
        "{log}"
    );
    assert!(
        log.contains("mise ls --installed --json go | yes=1 auto=false noconfig=1"),
        "{log}"
    );
    assert!(
        log.contains("uv tool list | noconfig=1 offline=1 downloads=never progress=1"),
        "{log}"
    );
    assert!(fx.changes().is_empty(), "{log}");
    let o = fx.packages(true, &digest_of(&o));
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    for op in [DUCKDB, GO, NUV] {
        assert!(out.contains(&format!("updated {op}")), "{out}");
    }
    assert!(out.contains(&format!("current {JAVA}")), "{out}");
    assert!(out.contains("result: ok"), "{out}");
    assert_eq!(
        fx.changes(),
        [
            "mise install duckdb",
            "mise install go@1.27",
            "uv tool upgrade nuv"
        ]
    );
    let log = fx.log();
    assert!(
        log.contains("mise install duckdb | yes=1 auto=false noconfig=1"),
        "{log}"
    );
    assert!(
        log.contains("uv tool upgrade nuv | noconfig=1 offline=unset downloads=never progress=1"),
        "{log}"
    );
    assert!(
        !log.contains("prune") && !log.contains("uninstall"),
        "{log}"
    );
    // Every version that was installed is still listed; the new ones are beside them.
    assert_eq!(
        fx.listed("mise"),
        "duckdb 1.5.5\nduckdb 1.5.6\ngo 1.27.1\njava 21.0.2\nduckdb 1.6.0\ngo 1.27.2\n"
    );
    assert_eq!(fx.listed("uv"), "nuv 2.0.0\n");
    let r = fx.receipts();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0]["command"], "packages");
    let ops = r[0]["operations"].as_array().unwrap();
    let status = |res: &str| ops.iter().find(|o| o["resource"] == res).unwrap()["status"].clone();
    assert_eq!(status("extra.duckdb"), "updated");
    assert_eq!(status("extra.java"), "current");
    // Another update with nothing newer changes nothing: the provider is asked, the listing
    // before and after is the same, so each tool is current. The receipt records the attempt.
    let o = fx.apply(true);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    for op in [DUCKDB, GO, NUV, JAVA] {
        assert!(out.contains(&format!("current {op}")), "{out}");
    }
    assert_eq!(fx.changes().len(), 6, "{}", fx.log());
    assert_eq!(fx.receipts().len(), 2);
}

#[test]
fn a_plain_packages_run_never_upgrades_a_rolling_tool_and_a_pin_change_installs_beside_the_old() {
    let fx = Fx::new();
    // Everything is installed, a newer duckdb exists: a sync-style run is current throughout.
    let o = fx.apply(false);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    for op in [DUCKDB, GO, JAVA, NUV] {
        assert!(out.contains(&format!("current {op}")), "{out}");
    }
    assert!(fx.changes().is_empty(), "{}", fx.log());
    assert!(fx.receipts().is_empty());
    // A new declared pin is installed; the old version stays and nothing else moves.
    fx.put(
        "base/modules/extra/module.toml",
        &EXTRA.replace("name = \"go@1.27\"", "name = \"go@1.28\""),
    );
    let o = fx.preview(false);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(text(&o.stdout).contains("install go@1.28 with mise/v1"));
    let o = fx.packages(false, &digest_of(&o));
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(text(&o.stdout).contains(&format!("updated {GO}")));
    assert_eq!(fx.changes(), ["mise install go@1.28"]);
    assert!(fx.listed("mise").contains("go 1.27.1\n"));
    assert!(fx.listed("mise").contains("go 1.28.0\n"));
    // A plain packages digest does not approve an update, and the other way round.
    let d = digest_of(&fx.preview(false));
    let o = fx.packages(true, &d);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PLAN_STALE"), "{}", stderr(&o));
    // `--update` belongs to packages only.
    for args in [
        &["secrets", "--root", fx.root_arg(), "--preview", "--update"][..],
        &["packages", "--root", fx.root_arg(), "--update", "--update"][..],
    ] {
        let o = fx.run(args);
        assert_eq!(code(&o), 2, "{}\n{}", stderr(&o), text(&o.stdout));
    }
}

#[test]
fn a_missing_runtime_blocks_the_plan_and_a_missing_interpreter_fails_the_upgrade() {
    let fx = Fx::new();
    // No uv on the fixed search path: the probe cannot run, and nothing installs one.
    fs::remove_file(fx.bin().join("uv")).unwrap();
    let o = fx.preview(true);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(stderr(&o).contains("E_PROBE_FAILED"), "{}", stderr(&o));
    assert!(text(&o.stdout).contains(&format!("blocked {NUV}")));
    assert!(fx.changes().is_empty(), "{}", fx.log());
    fs::write(fx.bin().join("uv"), FAKE_UV).unwrap();
    fs::set_permissions(fx.bin().join("uv"), fs::Permissions::from_mode(0o755)).unwrap();
    // uv finds no interpreter and may not download one: the upgrade fails, is journaled as
    // failed, and the listing is unchanged.
    fx.marker("nopython", "");
    let o = fx.apply(true);
    assert_eq!(code(&o), 1, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    assert!(out.contains(&format!("failed {NUV}")), "{out}");
    assert!(out.contains("uv tool upgrade nuv exited 2"), "{out}");
    assert!(!out.contains("No interpreter found"), "{out}");
    assert!(out.contains(&format!("updated {DUCKDB}")), "{out}");
    assert_eq!(fx.listed("uv"), "nuv 0.5.2\n");
    let journal = fs::read_to_string(fx.state().join("journal.jsonl")).unwrap();
    assert!(journal.contains("\"failed\""), "{journal}");
}

#[test]
fn a_copy_from_another_owner_is_reported_under_packages_and_blocks_the_update() {
    let fx = Fx::new();
    // A duckdb from another owner beside the mise one. A sync-style run leaves both alone: the
    // declared provider lists the tool, so it is current, not a broken install.
    let other = fx.other_copy("duckdb");
    let o = fx.preview(false);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(text(&o.stdout).contains(&format!("current {DUCKDB}")));
    // An update would deepen the duplicate ownership: it blocks until one owner is chosen.
    let o = fx.preview(true);
    assert_eq!(code(&o), 3, "{}\n{}", stderr(&o), text(&o.stdout));
    let e = stderr(&o);
    assert!(e.contains("E_CONFLICT"), "{e}");
    assert!(
        e.contains(&format!(
            "duckdb is declared for mise/v1 but an executable {} exists",
            other.display()
        )),
        "{e}"
    );
    assert!(fx.changes().is_empty(), "{}", fx.log());
    fs::remove_file(&other).unwrap();
    // The providers' own directories are not another owner: a mise shim and uv's entry point.
    let shims = fx.home.path().join(".local/share/mise/shims");
    fs::create_dir_all(&shims).unwrap();
    fs::write(shims.join("duckdb"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(shims.join("duckdb"), fs::Permissions::from_mode(0o755)).unwrap();
    fx.other_copy("nuv");
    let o = fx.preview(true);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let t = text(&o.stdout);
    assert!(t.contains(&format!("planned {DUCKDB}")), "{t}");
    assert!(t.contains(&format!("planned {NUV}")), "{t}");
}

#[test]
fn held_policies_local_builds_and_providers_without_a_rolling_path_are_kept() {
    let fx = Fx::new();
    // A git-ref build of duckdb beside the registry versions; a policy mise/v1 does not roll; a
    // homebrew/v1 package, which this build does not update.
    fx.installed(
        "mise",
        "duckdb 1.5.6\nduckdb ref:main\ngo 1.27.1\njava 21.0.2\n",
    );
    let cellar = fx.fake.path().join("Cellar/atuin/18.0.0");
    fs::create_dir_all(&cellar).unwrap();
    fs::write(
        cellar.join("INSTALL_RECEIPT.json"),
        r#"{"source":{"tap":"homebrew/core"}}"#,
    )
    .unwrap();
    fx.put(
        "base/modules/extra/module.toml",
        &format!(
            "{}\n[[resources]]\nid = \"atuin\"\nkind = \"package\"\nadapter = \"homebrew/v1\"\n[resources.value]\nname = \"atuin\"\nupdate_policy = \"rolling\"\n",
            EXTRA.replace(
                "name = \"go@1.27\"\nupdate_policy = \"rolling\"",
                "name = \"go@1.27\"\nupdate_policy = \"native-request\""
            )
        ),
    );
    let o = fx.apply(true);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    let out = text(&o.stdout);
    assert!(
        out.contains("duckdb is installed by mise/v1 and held (1.5.6 ref:main, ref:main is a local build, which is kept)"),
        "{out}"
    );
    assert!(
        out.contains(
            "go@1.27 is installed by mise/v1 and held (1.27.1, update_policy = native-request)"
        ),
        "{out}"
    );
    assert!(
        out.contains("atuin is installed by homebrew/v1 and held (18.0.0, homebrew/v1 has no rolling update path in this build)"),
        "{out}"
    );
    assert!(out.contains(&format!("updated {NUV}")), "{out}");
    assert_eq!(fx.changes(), ["uv tool upgrade nuv"]);
    assert_eq!(
        fx.listed("mise"),
        "duckdb 1.5.6\nduckdb ref:main\ngo 1.27.1\njava 21.0.2\n"
    );
}

#[test]
fn a_uv_source_that_is_not_an_index_name_is_refused_before_any_provider_runs() {
    let fx = Fx::new();
    // The first two pass the general package-name rule and are refused by the uv rule; the
    // rest fail the general rule. Every one is a manifest error, and no provider runs.
    for (i, source) in [
        "nuv@0.5",
        "pypi:nuv",
        "git+https://example.invalid/nuv",
        "./nuv",
        "nuv==0.5",
    ]
    .iter()
    .enumerate()
    {
        fx.put(
            "base/modules/extra/module.toml",
            &EXTRA.replace("name = \"nuv\"", &format!("name = \"{source}\"")),
        );
        for update in [false, true] {
            let o = fx.preview(update);
            assert_eq!(code(&o), 3, "{source}: {}\n{}", stderr(&o), text(&o.stdout));
            assert!(stderr(&o).contains("E_MANIFEST_INVALID"), "{}", stderr(&o));
            assert_eq!(
                stderr(&o).contains("uv-tool/v1 takes a package-index name"),
                i < 2,
                "{}",
                stderr(&o)
            );
        }
    }
    assert!(!fx.log().contains("uv "), "{}", fx.log());
}

#[test]
fn an_interrupted_upgrade_keeps_the_installed_versions_and_blocks_until_resolved() {
    let fx = Fx::new();
    let d = digest_of(&fx.preview(true));
    // The first upgrade (duckdb) finished, but the manager never recorded the outcome.
    let crash = fx
        .cmd(&[
            "packages",
            "--root",
            fx.root_arg(),
            "--yes-plan",
            &d,
            "--update",
        ])
        .env("DFM_FAILPOINT", "apply-after-effect:abort")
        .output()
        .unwrap();
    assert!(!crash.status.success());
    assert_eq!(fx.changes(), ["mise install duckdb"]);
    assert!(fx.listed("mise").contains("duckdb 1.5.6\n"));
    assert!(fx.listed("mise").contains("duckdb 1.6.0\n"));
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
    let id = intent["op_id"].as_str().unwrap().to_string();
    // Every later run, update or not, blocks the resource and runs no provider change for it.
    for update in [true, false] {
        let o = fx.preview(update);
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
        let o = fx.packages(update, &digest_of(&o));
        assert_ne!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
        assert!(stderr(&o).contains("E_OP_INTERRUPTED"), "{}", stderr(&o));
        assert!(stderr(&o).contains(&id), "{}", stderr(&o));
        assert!(text(&o.stdout).contains(&format!("blocked {DUCKDB}")));
    }
    assert_eq!(
        fx.changes().len(),
        1,
        "the unresolved upgrade must not run again"
    );
    let o = fx.run(&["state", "resolve", &id, "--as", "failed"]);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    // The newest version is already listed, so the next update leaves duckdb current.
    let o = fx.apply(true);
    assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    assert!(text(&o.stdout).contains(&format!("current {DUCKDB}")));
    assert!(text(&o.stdout).contains("result: ok"));
}

#[test]
fn a_read_only_plan_and_doctor_start_no_provider() {
    let fx = Fx::new();
    for cmd in ["plan", "doctor"] {
        let o = fx.run(&[cmd, "--root", fx.root_arg()]);
        assert_eq!(code(&o), 0, "{}\n{}", stderr(&o), text(&o.stdout));
    }
    assert!(
        !fx.fake.path().join("log").exists(),
        "a provider was started: {}",
        fx.log()
    );
}
