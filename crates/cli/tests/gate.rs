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
}

#[test]
fn only_the_contract_crates_are_direct_dependencies() {
    let manifest = fs::read_to_string(repo_root().join("Cargo.toml")).unwrap();
    let deps: Vec<&str> = manifest
        .split("[workspace.dependencies]")
        .nth(1)
        .unwrap()
        .lines()
        .take_while(|l| !l.starts_with('['))
        .filter(|l| l.contains('='))
        .map(|l| l.split('=').next().unwrap().trim())
        .collect();
    let allowed = [
        "dfm-model",
        "dfm-engine",
        "dfm-platform-macos",
        "serde",
        "serde_json",
        "toml",
        "sha2",
    ];
    assert!(deps.iter().all(|d| allowed.contains(d)), "{deps:?}");
    let pins: Vec<&str> = manifest
        .lines()
        .filter(|l| {
            ["serde =", "serde_json =", "toml =", "sha2 ="]
                .iter()
                .any(|k| l.starts_with(k))
        })
        .collect();
    assert_eq!(
        pins.len(),
        4,
        "expected exactly serde, serde_json, toml and sha2: {pins:?}"
    );
    assert!(
        pins.iter().all(|l| l.contains("\"=")),
        "direct crates need exact =x.y.z pins"
    );
}

#[test]
fn crate_dependency_direction_follows_the_contract() {
    // model -> nothing; platform_macos -> model; cli -> model, platform_macos. Cargo rejects
    // cycles, so checking the allowed edges is enough to keep the layering.
    let deps = |krate: &str| -> Vec<String> {
        let text =
            fs::read_to_string(repo_root().join("crates").join(krate).join("Cargo.toml")).unwrap();
        text.split("[dependencies]")
            .nth(1)
            .unwrap_or("")
            .lines()
            .take_while(|l| !l.starts_with('['))
            .filter_map(|l| l.split('=').next())
            .map(|d| d.trim().to_string())
            .filter(|d| d.starts_with("dfm-"))
            .collect()
    };
    assert!(deps("model").is_empty());
    assert_eq!(deps("engine"), ["dfm-model"]);
    assert_eq!(deps("platform_macos"), ["dfm-model"]);
    assert_eq!(deps("cli"), ["dfm-model", "dfm-platform-macos"]);
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
}
