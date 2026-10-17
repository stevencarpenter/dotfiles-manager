//! Structural gate tests named by docs/architecture.md sections 3, 7 and 12.

mod common;

use common::repo_root;
use std::fs;
use std::path::Path;
use std::process::Command;

fn rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn the_isolated_build_is_enabled_for_this_run() {
    // Through a variable: the value is a build-time constant, which is the point of the test.
    let enabled = std::hint::black_box(cfg!(feature = "test-isolation"));
    assert!(
        enabled,
        "run scripts/check_all.sh rust-tests; a plain cargo test skips the cli and build_controls targets (use --no-fail-fast to see every other failure)"
    );
}

#[test]
fn the_documented_fixture_count_is_the_real_one() {
    let doc = fs::read_to_string(repo_root().join("docs/bootstrap.md")).unwrap();
    let said: usize = doc
        .split("Shared checker fixtures (")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .expect("docs/bootstrap.md states the fixture count");
    let real = fs::read_dir(repo_root().join("tests/fixtures/build-controls/cases"))
        .unwrap()
        .filter(|e| e.as_ref().unwrap().path().is_dir())
        .count();
    assert_eq!(said, real, "update the count in docs/bootstrap.md");
}

#[test]
fn os_bound_names_stay_in_platform_macos() {
    let forbidden = [
        "/usr/bin",
        "/bin/",
        "/opt/homebrew",
        "/Users/",
        "Library/",
        "dscl",
        "NFSHomeDirectory",
        "launchctl",
        "homebrew",
        "xcode",
    ];
    // docs/architecture.md section 3: no OS-bound name in `model` or `engine`.
    let mut files = Vec::new();
    for krate in ["model", "engine"] {
        let before = files.len();
        rs_files(&repo_root().join(format!("crates/{krate}/src")), &mut files);
        assert!(files.len() > before, "no sources found for {krate}");
    }
    for f in files {
        // Compared without regard to case, so `Xcode` and `Homebrew` are caught too.
        let text = fs::read_to_string(&f).unwrap().to_lowercase();
        for needle in forbidden {
            assert!(
                !text.contains(&needle.to_lowercase()),
                "{} names {needle}",
                f.display()
            );
        }
    }
}

#[test]
fn bootstrap_script_literals_match_the_platform_crate() {
    let sh = fs::read_to_string(repo_root().join("scripts/bootstrap.sh")).unwrap();
    let value = |key: &str| -> String {
        let line = sh
            .lines()
            .find(|l| l.starts_with(&format!("{key}=")))
            .unwrap_or_else(|| panic!("{key} missing"));
        line[key.len() + 1..]
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    };
    assert_eq!(value("DATA_REL"), dfm_platform_macos::DATA_DIR);
    assert_eq!(value("CACHE_REL"), dfm_platform_macos::CACHE_DIR);
    assert_eq!(value("SYSPATH"), dfm_platform_macos::SYSTEM_PATH.join(":"));
}

#[test]
fn test_isolation_is_never_in_default_or_the_release_graph() {
    let manifest = fs::read_to_string(repo_root().join("crates/cli/Cargo.toml")).unwrap();
    assert!(
        !manifest
            .lines()
            .any(|l| l.trim_start().starts_with("default")),
        "cli declares default features"
    );
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let o = Command::new(cargo)
        .current_dir(repo_root())
        .args([
            "tree",
            "--locked",
            "--offline",
            "-p",
            "dfm-cli",
            "-e",
            "normal,build",
            "-f",
            "{p} {f}",
        ])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let tree = String::from_utf8_lossy(&o.stdout);
    assert!(tree.contains("dfm-cli"), "{tree}");
    assert!(
        !tree.contains("test-isolation"),
        "release graph enables test-isolation:\n{tree}"
    );
    // The engine's abort and wait code exists only with this feature.
    assert!(
        tree.contains("dfm-engine") && !tree.contains("failpoints"),
        "release graph enables failpoints:\n{tree}"
    );
}

/// The host triple, from `rustc -vV`. `cargo metadata --filter-platform` with it needs only the
/// packages a build for this machine downloads, so the test also runs offline with a fresh
/// CARGO_HOME after a plain build (without the filter it wants every target's packages).
fn host_triple() -> String {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let o = Command::new(rustc).arg("-vV").output().unwrap();
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .expect("rustc -vV prints a host line")
        .trim()
        .to_string()
}

fn metadata() -> serde_json::Value {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let o = Command::new(cargo)
        .current_dir(repo_root())
        .args([
            "metadata",
            "--locked",
            "--offline",
            "--format-version",
            "1",
            "--filter-platform",
        ])
        .arg(host_triple())
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}

/// Every direct dependency of every workspace member, of every kind (normal, dev, build) and
/// every target, must be in this table, with its exact requirement. Adding a crate, a dev
/// dependency or a target-specific dependency fails this test until the table is edited.
#[test]
fn the_direct_dependencies_of_every_member_match_the_allow_list_exactly() {
    use std::collections::{BTreeMap, BTreeSet};
    let m = metadata();
    let packages = m["packages"].as_array().unwrap();
    let members: BTreeSet<&str> = m["workspace_members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| {
            let p = packages.iter().find(|p| p["id"] == *id).unwrap();
            p["name"].as_str().unwrap()
        })
        .collect();
    let expected: BTreeMap<&str, Vec<&str>> = BTreeMap::from([
        (
            "dfm-model",
            vec![
                "normal:serde:=1.0.229",
                "normal:serde_json:=1.0.149",
                "normal:sha2:=0.10.9",
                "normal:toml:=1.1.6",
            ],
        ),
        (
            "dfm-engine",
            vec![
                "normal:dfm-model:*",
                "normal:libc:=0.2.190",
                "normal:rustix:=1.1.5",
            ],
        ),
        ("dfm-platform-macos", vec!["normal:dfm-model:*"]),
        (
            "dfm-cli",
            vec![
                "dev:serde_json:=1.0.149",
                "dev:toml:=1.1.6",
                "normal:dfm-engine:*",
                "normal:dfm-model:*",
                "normal:dfm-platform-macos:*",
                "normal:sha2:=0.10.9",
            ],
        ),
    ]);
    assert_eq!(
        members,
        expected.keys().copied().collect::<BTreeSet<_>>(),
        "a workspace member was added or removed: update the allow-list"
    );
    for p in packages
        .iter()
        .filter(|p| members.contains(p["name"].as_str().unwrap()))
    {
        let name = p["name"].as_str().unwrap();
        let mut got: Vec<String> = p["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| {
                assert!(
                    d["target"].is_null(),
                    "{name}: target-specific dependency {}",
                    d["name"]
                );
                format!(
                    "{}:{}:{}",
                    d["kind"].as_str().unwrap_or("normal"),
                    d["name"].as_str().unwrap(),
                    d["req"].as_str().unwrap()
                )
            })
            .collect();
        got.sort();
        assert_eq!(got, expected[name], "{name}: direct dependencies changed");
        // Nothing but the binary crate may depend on the binary crate, even for tests.
        assert!(
            name == "dfm-cli" || !got.iter().any(|g| g.contains(":dfm-cli:")),
            "{name} depends on dfm-cli"
        );
    }
}

/// libc is used by one module only (the signal handler). It is already in the lock file as
/// rustix's dependency. The engine asks for no default features.
#[test]
fn libc_is_one_exact_version_with_no_default_features_and_one_resolved_package() {
    let m = metadata();
    let ids = m["packages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["name"] == "libc")
        .count();
    assert_eq!(ids, 1, "exactly one libc version");
    let engine = m["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "dfm-engine")
        .unwrap();
    let dep = engine["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "libc")
        .unwrap();
    assert_eq!(dep["uses_default_features"], false);
    assert!(dep["features"].as_array().unwrap().is_empty());
}

/// The source of a file without `//` comments and without the text inside string literals (a
/// crude scan, a guard rail: the compiler lint is the enforcement). The keyword is built in the
/// tests, so this file does not contain it itself.
fn code_only(text: &str) -> String {
    let (mut out, mut in_str, mut chars) = (String::new(), false, text.chars().peekable());
    while let Some(c) = chars.next() {
        match (in_str, c) {
            (true, '\\') => {
                chars.next();
            }
            (true, '"') => in_str = false,
            (true, _) => {}
            (false, '"') => in_str = true,
            (false, '/') if chars.peek() == Some(&'/') => {
                for d in chars.by_ref() {
                    if d == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            (false, _) => out.push(c),
        }
    }
    out
}

fn has_keyword(code: &str, word: &str) -> bool {
    code.match_indices(word).any(|(i, _)| {
        let before = code[..i].chars().next_back();
        let after = code[i + word.len()..].chars().next();
        let ident = |c: char| c.is_alphanumeric() || c == '_';
        !before.is_some_and(ident) && !after.is_some_and(ident)
    })
}

/// DFM-47: the lint attribute layer. Text that names the lint may appear only in the crate roots
/// and in the signal module, and the exception is one `expect` on `install`. This reads raw
/// text, so a comment, a raw string or a character literal cannot hide an attribute.
#[test]
fn the_unsafe_lint_is_named_only_in_the_crate_roots_and_on_the_two_signal_functions() {
    let lint = ["un", "safe_code"].concat();
    let roots = [
        "crates/model/src/lib.rs",
        "crates/platform_macos/src/lib.rs",
        "crates/cli/src/lib.rs",
        "crates/cli/src/main.rs",
    ];
    let mut files = Vec::new();
    rs_files(&repo_root().join("crates"), &mut files);
    for f in &files {
        let rel = f.strip_prefix(repo_root()).unwrap().display().to_string();
        let t = fs::read_to_string(f).unwrap();
        let named = t.contains(&lint);
        let allowed = roots.contains(&rel.as_str())
            || rel == "crates/engine/src/lib.rs"
            || rel == "crates/engine/src/signal.rs";
        assert!(!named || allowed, "{rel} names {lint}");
    }
    let forbid = format!("#![forbid({lint})]");
    for root in roots {
        let t = fs::read_to_string(repo_root().join(root)).unwrap();
        assert_eq!(t.matches(&lint).count(), 1, "{root}");
        assert!(t.contains(&forbid), "{root} lacks {forbid}");
    }
    let engine = fs::read_to_string(repo_root().join("crates/engine/src/lib.rs")).unwrap();
    assert_eq!(engine.matches(&lint).count(), 1);
    assert!(engine.contains(&format!("#![deny({lint})]")));
    // The signal module: one `expect` each, directly on `pub fn install` (the handlers) and on
    // `pub(crate) fn kill_group` (ending a provider child, DFM-22), and no `allow`.
    let module = fs::read_to_string(repo_root().join("crates/engine/src/signal.rs")).unwrap();
    let flat: String = module.split_whitespace().collect::<Vec<_>>().join(" ");
    let functions = ["pub fn install()", "pub(crate) fn kill_group("];
    assert_eq!(module.matches(&lint).count(), functions.len(), "{module}");
    assert!(
        !flat.contains(&format!("allow({lint}")),
        "signal.rs allows {lint}"
    );
    let mut rest = flat.as_str();
    for want in functions {
        let at = rest
            .find(&format!("#[expect( {lint}, reason = "))
            .or_else(|| rest.find(&format!("#[expect({lint}, reason = ")))
            .unwrap_or_else(|| panic!("signal.rs lacks the expect attribute for {want}"));
        let after = &rest[at..];
        let end = after.find("fn ").unwrap();
        // The attribute sits directly on the function: nothing else is declared between them.
        assert!(
            after[..end].matches("fn ").count() == 0
                && after[end..].starts_with(
                    want.trim_start_matches("pub(crate) ")
                        .trim_start_matches("pub ")
                ),
            "the expect is not on {want}: {}",
            &after[..end.min(200)]
        );
        rest = &after[end..];
    }
    let manifest = fs::read_to_string(repo_root().join("Cargo.toml")).unwrap();
    assert!(manifest.contains(&format!("{lint} = \"deny\"")));
}

/// DFM-47: the keyword layer, a second check behind the lint layer. Comments and string literals
/// are stripped by a crude scan.
#[test]
fn the_unsafe_keyword_appears_only_in_the_engine_signal_module() {
    let word = ["un", "safe"].concat();
    let mut files = Vec::new();
    rs_files(&repo_root().join("crates"), &mut files);
    let mut seen = Vec::new();
    for f in &files {
        if has_keyword(&code_only(&fs::read_to_string(f).unwrap()), &word) {
            seen.push(f.strip_prefix(repo_root()).unwrap().display().to_string());
        }
    }
    assert_eq!(seen, ["crates/engine/src/signal.rs"], "{seen:?}");
    // Every block carries its safety argument.
    let module = fs::read_to_string(repo_root().join("crates/engine/src/signal.rs")).unwrap();
    let blocks = module.matches(&format!("{word} {{")).count();
    assert!(blocks > 0 && module.matches("// SAFETY:").count() >= blocks);
}

/// DFM-47 C03: only the mutating path of `apply` installs the handlers. The call is in one place
/// and it comes after the preview return, the lock and no earlier.
#[test]
fn only_the_mutating_path_of_apply_installs_the_signal_handlers() {
    let mut files = Vec::new();
    rs_files(&repo_root().join("crates"), &mut files);
    let callers: Vec<_> = files
        .iter()
        .filter(|f| !f.ends_with("crates/engine/src/signal.rs") && !f.ends_with("tests/gate.rs"))
        .filter(|f| fs::read_to_string(f).unwrap().contains("signal::install("))
        .map(|f| f.strip_prefix(repo_root()).unwrap().display().to_string())
        .collect();
    assert_eq!(callers, ["crates/cli/src/apply_cmd.rs"]);
    let t = fs::read_to_string(repo_root().join("crates/cli/src/apply_cmd.rs")).unwrap();
    let install = t.find("signal::install(").unwrap();
    assert_eq!(t.matches("signal::install(").count(), 1);
    assert!(t.find("if args.preview {").unwrap() < install);
    assert!(t.find("Store::open_write_with(").unwrap() < install);
    // After the second plan (which runs `id -u`), so no child process starts after the install.
    assert!(t.find("let locked = plan_for(").unwrap() < install);
    assert!(install < t.find("apply(&run, &locked)").unwrap());
}

/// rustix is the one crate allowed to reach the OS, through its safe fs API only. Its resolved
/// features must be exactly these. A deny-list would miss a new feature.
#[test]
fn rustix_is_resolved_with_exactly_the_alloc_fs_and_std_features() {
    let m = metadata();
    let ids: Vec<&serde_json::Value> = m["packages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["name"] == "rustix")
        .map(|p| &p["id"])
        .collect();
    assert_eq!(ids.len(), 1, "exactly one rustix version: {ids:?}");
    let node = m["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == *ids[0])
        .unwrap();
    let mut features: Vec<&str> = node["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    features.sort_unstable();
    assert_eq!(features, ["alloc", "fs", "std"]);
    // And the requested features and default-features flag of the one edge that names it.
    let engine = m["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "dfm-engine")
        .unwrap();
    let dep = engine["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "rustix")
        .unwrap();
    assert_eq!(dep["uses_default_features"], false);
    let mut asked: Vec<&str> = dep["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    asked.sort_unstable();
    assert_eq!(asked, ["fs", "std"]);
}

/// The environment variables only a `test-isolation` build reads.
const ISOLATION_NAMES: [&str; 6] = [
    "DFM_WIDTH",
    "DFM_FAILPOINT",
    "DFM_HOME",
    "DFM_BINDING",
    "DFM_PLATFORM",
    "DFM_PROVIDER_PATH",
];

#[test]
fn a_plain_release_build_of_every_target_has_no_test_isolation() {
    let target = common::TempDir::new("release-graph");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let o = Command::new(cargo)
        .current_dir(repo_root())
        .args([
            "build",
            "--release",
            "--workspace",
            "--all-targets",
            "--locked",
            "--offline",
            "--target-dir",
        ])
        .arg(target.path())
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v = Command::new(target.path().join("release/dotfiles-manager"))
        .arg("--version")
        .output()
        .unwrap();
    let v = String::from_utf8_lossy(&v.stdout);
    assert!(
        v.starts_with("dotfiles-manager ") && !v.contains("test-isolation"),
        "{v}"
    );
    // The isolation variables are compiled out: `width()` and `fail_from_env()` have no name to
    // read. The scan has a positive control, the isolated test binary, which must name all of
    // them, so a renamed or run-time-built variable cannot make the release scan pass vacuously.
    let release = fs::read(target.path().join("release/dotfiles-manager")).unwrap();
    let isolated = fs::read(env!("CARGO_BIN_EXE_dotfiles-manager")).unwrap();
    let names_in = |bin: &[u8]| {
        ISOLATION_NAMES
            .into_iter()
            .filter(|n| bin.windows(n.len()).any(|w| w == n.as_bytes()))
            .collect::<Vec<_>>()
    };
    assert_eq!(names_in(&isolated), ISOLATION_NAMES, "the control binary");
    assert_eq!(names_in(&release), Vec::<&str>::new(), "the release binary");
    // The release `width()` is the default. `DFM_WIDTH` narrows the isolated table and does
    // nothing to the release one. HOME is a scratch directory that is not the account home, so
    // `doctor` stops at the HOME check and reads and writes nothing else.
    let home = common::TempDir::new("release-width");
    let doctor = |bin: &Path, width: Option<&str>| {
        let mut c = Command::new(bin);
        c.env_clear().env("HOME", home.path());
        if let Some(w) = width {
            c.env("DFM_WIDTH", w);
        }
        c.args(["doctor", "--root"])
            .arg(home.path())
            .output()
            .unwrap()
    };
    let release_bin = target.path().join("release/dotfiles-manager");
    let (plain, narrow) = (doctor(&release_bin, None), doctor(&release_bin, Some("20")));
    assert_eq!(plain.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&plain.stdout).contains("E_HOME_MISMATCH"));
    assert_eq!(
        plain.stdout, narrow.stdout,
        "the release width ignores DFM_WIDTH"
    );
    assert_eq!(
        fs::read_dir(home.path()).unwrap().count(),
        0,
        "nothing was created"
    );
    let isolated_doctor = |width: Option<&str>| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_dotfiles-manager"));
        c.env_clear().args(["--home", "/a b"]);
        if let Some(w) = width {
            c.env("DFM_WIDTH", w);
        }
        c.args(["doctor", "--root"])
            .arg(home.path())
            .output()
            .unwrap()
            .stdout
    };
    assert_ne!(
        isolated_doctor(None),
        isolated_doctor(Some("20")),
        "control: DFM_WIDTH changes the isolated table"
    );
    // The release binary has no `--home` flag, so it is a usage error before anything is read
    // or written. This does not run `apply`: it proves only that the flag is absent.
    let scratch = common::TempDir::new("release-apply");
    let o = Command::new(target.path().join("release/dotfiles-manager"))
        .env_clear()
        .env("DFM_HOME", scratch.path())
        .arg("--home")
        .arg(scratch.path())
        .args(["apply", "--root"])
        .arg(scratch.path())
        .output()
        .unwrap();
    assert_eq!(
        o.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(
        fs::read_dir(scratch.path()).unwrap().count(),
        0,
        "nothing was created"
    );
}
