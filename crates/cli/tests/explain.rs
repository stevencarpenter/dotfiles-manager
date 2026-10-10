//! `explain` in an isolated HOME against copies of the fixtures: output, refusals, read-only.

mod common;

use common::{TempDir, copy_tree, repo_root};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_dotfiles-manager");

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .env_clear()
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// A temp copy of the examples, so no test reads or could write the checked-in files.
fn examples_copy() -> (TempDir, PathBuf) {
    let t = TempDir::new("examples");
    let root = t.path().join("examples");
    copy_tree(&repo_root().join("examples"), &root);
    (t, root)
}

fn bound(profile: &str) -> (TempDir, TempDir, PathBuf) {
    let home = TempDir::new("home");
    let (t, root) = examples_copy();
    let p = root.join("profiles").join(profile);
    let o = run(
        home.path(),
        &[
            "bind",
            "--profile",
            p.to_str().unwrap(),
            "--machine-id",
            "m5-personal",
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    (home, t, root)
}

fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    fn go(d: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let mut v: Vec<_> = fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        v.sort();
        for p in v {
            if p.is_dir() {
                out.push((p.clone(), Vec::new()));
                go(&p, out);
            } else {
                out.push((p.clone(), fs::read(&p).unwrap()));
            }
        }
    }
    go(dir, &mut out);
    out
}

#[test]
fn explain_shows_ownership_provenance_and_native_precedence() {
    let (home, _t, root) = bound("work-01.toml");
    let r = root.to_str().unwrap();
    let o = run(home.path(), &["explain", "desktop.aerospace", "--root", r]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    for part in [
        "isolated: true",
        "profile work-01",
        "declared by module desktop",
        "permitted overrides: /source",
        "/source = \"files/aerospace.toml\"",
        "owner desktop",
        "changed by work (override)",
        "reason: Supply a complete variant through the exposed source slot.",
        "supplied by external/modules/work/module.toml",
        "source file external/modules/work/files/aerospace.toml",
        "content sha256:",
        "runtime verification: not verified",
    ] {
        assert!(out.contains(part), "missing {part:?} in:\n{out}");
    }
    let o = run(home.path(), &["explain", "core.git-defaults", "--root", r]);
    let out = text(&o.stdout);
    assert!(out.contains("native precedence:"), "{out}");
    assert!(out.contains("/core/editor = \"vim\""), "{out}");
    assert!(out.contains("declared value: \"nvim\""), "{out}");
}

#[test]
fn explain_finds_a_resource_by_output_path_without_regard_to_case() {
    let (home, _t, root) = bound("personal-dev-01.toml");
    let r = root.to_str().unwrap();
    for q in [
        ".config/git/config",
        ".Config/GIT/config",
        ".claude/skills/sample-audit",
    ] {
        let o = run(home.path(), &["explain", "--root", r, q]);
        assert_eq!(o.status.code(), Some(0), "{q}: {}", text(&o.stderr));
    }
    let o = run(home.path(), &["explain", ".config/git/config", "--root", r]);
    assert!(text(&o.stdout).contains("resource core.git\n"));
}

#[test]
fn explain_redacts_secret_references_and_shows_no_hash_of_them() {
    let (home, _t, root) = bound("personal-dev-01.toml");
    let o = run(
        home.path(),
        &[
            "explain",
            "personal-dev.example-secret",
            "--root",
            root.to_str().unwrap(),
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(out.contains("[redacted]"), "{out}");
    assert!(!out.contains("example-account"), "{out}");
    assert!(!out.contains("content sha256"), "{out}");
}

#[test]
fn explain_writes_nothing() {
    let (home, _t, root) = bound("work-01.toml");
    let (h0, r0) = (snapshot(home.path()), snapshot(&root));
    for q in ["desktop.aerospace", "core.git", "nonexistent.thing"] {
        run(
            home.path(),
            &["explain", q, "--root", root.to_str().unwrap()],
        );
    }
    assert_eq!(snapshot(home.path()), h0, "HOME changed");
    assert_eq!(snapshot(&root), r0, "sources changed");
}

#[test]
fn explain_refusals_have_a_stable_code_and_exit() {
    let (home, _t, root) = bound("work-01.toml");
    let r = root.to_str().unwrap();
    // Usage errors.
    for args in [
        vec!["explain"],
        vec!["explain", "core.git"],
        vec!["explain", "--root", r],
        vec!["explain", "a", "b", "--root", r],
        vec!["explain", "core.git", "--root"],
        vec!["explain", "core.git", "--root", r, "--root", r],
        vec!["explain", "core.git", "--bogus", "--root", r],
        vec!["explain", "no.such.resource", "--root", r],
    ] {
        let o = run(home.path(), &args);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", text(&o.stderr));
        assert!(text(&o.stderr).starts_with("error: E_USAGE"), "{args:?}");
    }
    // A resolver refusal: an unknown key in a module.
    let module = root.join("external/modules/work/module.toml");
    let mut body = fs::read_to_string(&module).unwrap();
    body = body.replacen("requires = [", "bogus = 1\nrequires = [", 1);
    fs::write(&module, body).unwrap();
    let o = run(home.path(), &["explain", "core.git", "--root", r]);
    assert_eq!(o.status.code(), Some(3));
    let err = text(&o.stderr);
    assert!(err.starts_with("error: E_FIELD_UNKNOWN"), "{err}");
    assert!(err.contains("external/modules/work/module.toml"), "{err}");
    assert!(err.contains("/bogus"), "{err}");
}

#[test]
fn explain_needs_a_binding_and_a_profile_inside_the_root() {
    let home = TempDir::new("unbound");
    let (_t, root) = examples_copy();
    let o = run(
        home.path(),
        &["explain", "core.git", "--root", root.to_str().unwrap()],
    );
    assert_eq!(o.status.code(), Some(3));
    assert!(
        text(&o.stderr).contains("E_BINDING_INVALID"),
        "{}",
        text(&o.stderr)
    );
    assert!(snapshot(home.path()).is_empty());

    let (home, _t, _root) = bound("work-01.toml");
    let other = TempDir::new("elsewhere");
    let o = run(
        home.path(),
        &[
            "explain",
            "core.git",
            "--root",
            other.path().to_str().unwrap(),
        ],
    );
    assert_eq!(o.status.code(), Some(3));
    assert!(
        text(&o.stderr).contains("E_PROFILE_INVALID"),
        "{}",
        text(&o.stderr)
    );
}

#[test]
fn help_lists_explain() {
    let o = Command::new(BIN)
        .env_clear()
        .arg("--help")
        .output()
        .unwrap();
    assert!(text(&o.stdout).contains("explain <resource-or-target> --root <dir>"));
}
