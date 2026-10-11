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
    let mut files = Vec::new();
    rs_files(&repo_root().join("crates/model/src"), &mut files);
    assert!(!files.is_empty());
    for f in files {
        let text = fs::read_to_string(&f).unwrap();
        // Compare outside `#[cfg(test)]` modules and comments only by simple containment.
        for needle in forbidden {
            assert!(!text.contains(needle), "{} names {needle}", f.display());
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
            vec!["normal:dfm-model:*", "normal:rustix:=1.1.5"],
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
    // The release binary has no isolation flag, so `apply` with an override is a usage error
    // before anything is read or written, and the failure-injection variable has no effect.
    let scratch = common::TempDir::new("release-apply");
    let o = Command::new(target.path().join("release/dotfiles-manager"))
        .env_clear()
        .env("DFM_HOME", scratch.path())
        .env("DFM_FAILPOINT", "apply-before-effect:abort")
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
