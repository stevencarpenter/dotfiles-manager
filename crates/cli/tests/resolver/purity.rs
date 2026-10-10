//! C05 and C06: resolution is deterministic and pure, and leaves everything as it found it.

use crate::cases::{cases, run};
use crate::common::{TempDir, repo_root};
use crate::harness::{edit, examples, synthetic};
use dfm_cli::builtin_catalog;
use dfm_model::Code;
use dfm_model::resolve::resolve;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// Names, modification times and bytes of everything below `root`.
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

#[test]
fn c05_independently_built_inputs_give_identical_results() {
    // Each call loads the fixtures from disk into a new set, so nothing is shared between runs.
    let cat = builtin_catalog();
    for (profile, load) in [
        ("profiles/machine-b.toml", synthetic as fn() -> _),
        ("profiles/work-01.toml", examples as fn() -> _),
    ] {
        let first = resolve(&load(), profile, &cat).unwrap();
        let second = resolve(&load(), profile, &cat).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            format!("{first:?}"),
            format!("{second:?}"),
            "the debug form is identical too, so no map order leaks"
        );
    }
}

#[test]
fn c05_when_two_problems_exist_the_same_one_is_always_reported() {
    let cat = builtin_catalog();
    let a = "profiles/machine-a.toml";
    let b = "profiles/machine-b.toml";
    // A phase 1 problem beats a phase 2 problem, whichever the files list first.
    let mut set = synthetic();
    edit(
        &mut set,
        "shared/reusable/module.toml",
        "add",
        "/bogus",
        Some("1"),
    );
    edit(&mut set, a, "add", "/modules/-", Some("\"ghost\""));
    let e = resolve(&set, a, &cat).unwrap_err();
    assert_eq!(
        (e.code, e.source.as_str()),
        (Code::FieldUnknown, "shared/reusable/module.toml")
    );

    // Two malformed modules: the one with the smaller ID is loaded and reported first, however
    // the profile orders its selection.
    for modules in [
        "[\"private-mod\"]",
        "[\"reusable\", \"private-mod\"]",
        "[\"private-mod\", \"reusable\"]",
    ] {
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
            "private/private-mod/module.toml",
            "add",
            "/bogus",
            Some("1"),
        );
        edit(&mut set, b, "replace", "/modules", Some(modules));
        let e = resolve(&set, b, &cat).unwrap_err();
        assert_eq!(
            (e.code, e.source.as_str()),
            (Code::FieldUnknown, "private/private-mod/module.toml"),
            "{modules}"
        );
    }

    // Two forbidden overrides: the smaller resource ID is reported, whichever is declared first.
    for order in [
        ["core.launcher", "reusable.tool-config"],
        ["reusable.tool-config", "core.launcher"],
    ] {
        let mut set = synthetic();
        for r in order {
            let o = format!(
                "{{ resource = \"{r}\", path = \"/source\", value = \"x\", reason = \"t\" }}"
            );
            edit(&mut set, b, "add", "/overrides/-", Some(&o));
        }
        // reusable.tool-config exposes /source, so only core.launcher is forbidden; make both bad.
        edit(
            &mut set,
            "shared/reusable/module.toml",
            "remove",
            "/resources/0/policy",
            None,
        );
        let e = resolve(&set, b, &cat).unwrap_err();
        assert_eq!(
            (e.code, e.resource.as_deref()),
            (Code::OverrideForbidden, Some("core.launcher")),
            "{order:?}"
        );
    }
}

#[test]
fn c05_the_model_crate_has_no_io_process_network_or_env_access() {
    // A source scan. It shows the code that is there does not name these APIs. It is not a
    // proof about dependencies, which the manifest check below pins.
    let mut files = Vec::new();
    fn walk(d: &Path, out: &mut Vec<PathBuf>) {
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
    let banned_paths = [
        "std::fs",
        "std::io",
        "std::process",
        "std::net",
        "std::env",
        "std::os",
        "std::thread",
        "std::time",
        "std::sync",
    ];
    let banned_names = [
        "fs", "io", "process", "net", "env", "os", "thread", "time", "sync",
    ];
    let banned_words = [
        "Command",
        "TcpStream",
        "UdpSocket",
        "File::",
        "read_to_string",
        "read_dir",
        "current_dir",
        "getenv",
        "extern \"C\"",
        "unsafe {",
        "unsafe fn",
        "unsafe impl",
        "unsafe trait",
        "unsafe extern",
    ];
    for f in files {
        let text = fs::read_to_string(&f).unwrap();
        let name = f.display();
        for b in banned_paths.iter().chain(&banned_words) {
            assert!(!text.contains(b), "{name} uses {b}");
        }
        // Grouped imports such as `use std::{fs, collections::BTreeMap}` hide the path.
        let mut rest = text.as_str();
        while let Some(i) = rest.find("std::{") {
            let group = &rest[i + 6..];
            let end = group.find('}').expect("a closed import group");
            for item in group[..end].split(',') {
                let head = item.trim().split("::").next().unwrap_or("");
                assert!(
                    !banned_names.contains(&head),
                    "{name} imports std::{head} in a group"
                );
            }
            rest = &group[end..];
        }
    }
    // Only the data crates are direct dependencies of the model.
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
    assert!(
        manifest.contains("workspace = true"),
        "the model inherits the workspace lints, which forbid unsafe code"
    );
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
        "the canary directory stayed empty"
    );
}

#[test]
fn c06_all_sixteen_definitions_leave_the_fixture_files_untouched() {
    let roots = [
        repo_root().join("examples"),
        repo_root().join("tests/fixtures/resolver"),
    ];
    let before: Vec<String> = roots.iter().map(|r| tree_fingerprint(r)).collect();
    for case in cases() {
        let _ = run(&case);
    }
    // The synthetic profiles too, and every example profile directly.
    for p in ["machine-a", "machine-b"] {
        let _ = resolve(
            &synthetic(),
            &format!("profiles/{p}.toml"),
            &builtin_catalog(),
        );
    }
    let after: Vec<String> = roots.iter().map(|r| tree_fingerprint(r)).collect();
    assert_eq!(before, after);
}

#[test]
fn c06_resolution_does_not_mutate_the_set_it_reads() {
    let cat = builtin_catalog();
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
}
