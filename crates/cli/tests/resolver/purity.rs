//! C05 and C06: resolution is deterministic and pure, and leaves everything as it found it.

use crate::common::{TempDir, repo_root};
use crate::harness::{edit, examples, synthetic};
use dfm_cli::builtin_catalog;
use dfm_model::resolve::{SourceSet, resolve};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

fn tree_fingerprint(root: &Path) -> String {
    fn go(dir: &Path, h: &mut Sha256) {
        let mut v: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        v.sort_by_key(|e| e.file_name());
        for e in v {
            let m = fs::symlink_metadata(e.path()).unwrap();
            h.update(e.path().to_str().unwrap().as_bytes());
            h.update(format!("{:?}", m.modified().unwrap()).as_bytes());
            if m.is_dir() {
                go(&e.path(), h);
            } else if m.is_file() {
                h.update(fs::read(e.path()).unwrap());
            }
        }
    }
    let mut h = Sha256::new();
    go(root, &mut h);
    format!("{:x}", h.finalize())
}

fn rebuilt_in_reverse(set: &SourceSet) -> SourceSet {
    let mut out = SourceSet::new();
    let mut paths: Vec<&str> = set.paths().collect();
    paths.reverse();
    for p in paths {
        match set.get(p).unwrap() {
            dfm_model::resolve::Entry::File(b) => out.insert_file(p, b.clone()),
            dfm_model::resolve::Entry::Link(t) => out.insert_link(p, t.clone()),
        }
    }
    out
}

#[test]
fn c05_resolution_is_deterministic() {
    let cat = builtin_catalog();
    for (set, profile) in [
        (synthetic(), "profiles/machine-b.toml"),
        (examples(), "profiles/work-01.toml"),
    ] {
        let first = resolve(&set, profile, &cat).unwrap();
        assert_eq!(first, resolve(&set, profile, &cat).unwrap());
        assert_eq!(
            first,
            resolve(&rebuilt_in_reverse(&set), profile, &cat).unwrap()
        );
    }
    // The first error is deterministic too.
    let mut set = synthetic();
    edit(
        &mut set,
        "shared/reusable/module.toml",
        "add",
        "/bogus",
        Some("1"),
    );
    edit(
        &mut set,
        "shared/core/module.toml",
        "add",
        "/bogus",
        Some("1"),
    );
    let a = resolve(&set, "profiles/machine-a.toml", &cat).unwrap_err();
    assert_eq!(
        a,
        resolve(&rebuilt_in_reverse(&set), "profiles/machine-a.toml", &cat).unwrap_err()
    );
}

#[test]
fn c05_the_model_crate_has_no_io_process_network_or_env_access() {
    let banned = [
        "std::fs",
        "std::process",
        "std::net",
        "std::env",
        "std::os",
        "std::thread",
        "Command",
        "TcpStream",
        "UdpSocket",
        "File::",
        "read_to_string",
        "read_dir",
        "current_dir",
        "getenv",
    ];
    let mut files = Vec::new();
    fn walk(d: &Path, out: &mut Vec<std::path::PathBuf>) {
        for e in fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    walk(&repo_root().join("crates/model/src"), &mut files);
    assert!(files.len() > 10);
    for f in files {
        let text = fs::read_to_string(&f).unwrap();
        for b in banned {
            assert!(!text.contains(b), "{} uses {b}", f.display());
        }
    }
    // Only the data crates: no network, process or package library can be linked in.
    let manifest = fs::read_to_string(repo_root().join("crates/model/Cargo.toml")).unwrap();
    let deps: Vec<&str> = manifest
        .split("[dependencies]")
        .nth(1)
        .unwrap()
        .lines()
        .take_while(|l| !l.starts_with('['))
        .filter_map(|l| l.split('=').next())
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .collect();
    assert_eq!(deps, ["serde", "toml", "sha2"]);
}

#[test]
fn c05_manifest_text_is_data_and_never_executed() {
    let home = TempDir::new("canary");
    let canary = home.path().join("pwned");
    let mut set = synthetic();
    let argv = format!(
        "[\"touch\", \"{}\", \"$(touch {})\", \"`id`\"]",
        canary.display(),
        canary.display()
    );
    let handler = format!("{{ event = \"SessionStart\", type = \"command\", argv = {argv} }}");
    edit(
        &mut set,
        "shared/reusable/module.toml",
        "add",
        "/resources/3/value/handlers/reusable.hook",
        Some(&handler),
    );
    edit(
        &mut set,
        "shared/reusable/module.toml",
        "replace",
        "/resources/2/value/tool/paths",
        Some(&argv),
    );
    let r = resolve(&set, "profiles/machine-a.toml", &builtin_catalog()).unwrap();
    assert!(
        r.resources["reusable.agent-settings"].value["handlers"]["reusable.hook"]["argv"]
            .is_array()
    );
    assert!(!canary.exists());
    assert_eq!(
        fs::read_dir(home.path()).unwrap().count(),
        0,
        "resolution wrote into the directory"
    );
}

#[test]
fn c06_resolution_does_not_mutate_its_inputs_on_success_or_failure() {
    let cat = builtin_catalog();
    let before_disk = [
        tree_fingerprint(&repo_root().join("examples")),
        tree_fingerprint(&repo_root().join("tests/fixtures/resolver")),
    ];
    let home = TempDir::new("home");
    let before_home = tree_fingerprint(home.path());

    let ok_set = synthetic();
    let ok_copy = ok_set.clone();
    resolve(&ok_set, "profiles/machine-b.toml", &cat).unwrap();
    assert_eq!(ok_set, ok_copy);

    let mut bad_set = synthetic();
    edit(
        &mut bad_set,
        "shared/reusable/module.toml",
        "add",
        "/bogus",
        Some("1"),
    );
    let bad_copy = bad_set.clone();
    resolve(&bad_set, "profiles/machine-a.toml", &cat).unwrap_err();
    assert_eq!(bad_set, bad_copy);

    // The 16 example definitions leave the checked-in files and an isolated HOME untouched.
    for profile in ["personal-dev-01", "work-01", "homelab-01", "server-01"] {
        let _ = resolve(&examples(), &format!("profiles/{profile}.toml"), &cat);
    }
    assert_eq!(
        before_disk,
        [
            tree_fingerprint(&repo_root().join("examples")),
            tree_fingerprint(&repo_root().join("tests/fixtures/resolver")),
        ]
    );
    assert_eq!(before_home, tree_fingerprint(home.path()));
}
