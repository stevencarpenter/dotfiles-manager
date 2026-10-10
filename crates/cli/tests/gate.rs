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
    let allowed = ["dfm-model", "dfm-platform-macos", "serde", "toml", "sha2"];
    assert!(deps.iter().all(|d| allowed.contains(d)), "{deps:?}");
    let pinned = manifest
        .lines()
        .filter(|l| l.starts_with("serde =") || l.starts_with("toml =") || l.starts_with("sha2 ="))
        .all(|l| l.contains("\"="));
    assert!(pinned, "direct crates need exact =x.y.z pins");
}
