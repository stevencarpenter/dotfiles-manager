//! `explain` in an isolated HOME against copies of the fixtures: output, refusals, read-only.

mod common;

use common::{TempDir, copy_tree, repo_root};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_dfm");

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .env_clear()
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .unwrap()
}

/// Runs a command and kills it if it has not finished in `secs`, so a command that blocks on a
/// FIFO fails the test instead of hanging the gate.
fn finish_within(mut cmd: Command, secs: u64) -> Output {
    use std::process::Stdio;
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed().as_secs() >= secs {
            let _ = child.kill();
            panic!("command did not finish within {secs} s: {cmd:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
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
            "--root",
            root.to_str().unwrap(),
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

/// Every `<module>.<resource>` ID declared by the example modules.
fn resource_ids(root: &Path) -> Vec<String> {
    let mut ids = Vec::new();
    fn go(dir: &Path, ids: &mut Vec<String>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                go(&p, ids);
            } else if p.file_name().is_some_and(|n| n == "module.toml") {
                let doc: toml::Table = fs::read_to_string(&p).unwrap().parse().unwrap();
                let module = doc["id"].as_str().unwrap().to_string();
                for r in doc
                    .get("resources")
                    .and_then(|r| r.as_array())
                    .into_iter()
                    .flatten()
                {
                    ids.push(format!("{module}.{}", r["id"].as_str().unwrap()));
                }
            }
        }
    }
    go(root, &mut ids);
    ids.sort();
    ids
}

#[test]
fn explain_writes_nothing_for_any_profile_or_resource() {
    let ids = resource_ids(&repo_root().join("examples"));
    assert!(ids.len() >= 14, "{ids:?}");
    let checked_in = snapshot(&repo_root().join("examples"));
    // personal-dev-01 and work-01 are macOS profiles that resolve; the Linux ones are refused
    // at bind on this build, so they are covered by the resolver tests instead.
    for profile in ["personal-dev-01.toml", "work-01.toml"] {
        let (home, _t, root) = bound(profile);
        let (h0, r0) = (snapshot(home.path()), snapshot(&root));
        let mut explained = 0;
        for id in &ids {
            let o = run(
                home.path(),
                &["explain", id, "--root", root.to_str().unwrap()],
            );
            // A resource of a module the profile does not select is a usage error, not a write.
            assert!(
                matches!(o.status.code(), Some(0 | 2)),
                "{profile} {id}: {}",
                text(&o.stderr)
            );
            explained += usize::from(o.status.code() == Some(0));
        }
        assert!(explained >= 8, "{profile}: only {explained} explained");
        assert_eq!(snapshot(home.path()), h0, "{profile}: HOME changed");
        assert_eq!(snapshot(&root), r0, "{profile}: sources changed");
    }
    assert_eq!(
        snapshot(&repo_root().join("examples")),
        checked_in,
        "the checked-in examples changed"
    );
}

#[test]
fn explain_prints_the_include_order_of_a_native_document() {
    let (home, _t, root) = bound("work-01.toml");
    let o = run(
        home.path(),
        &[
            "explain",
            "work.git-identity",
            "--root",
            root.to_str().unwrap(),
        ],
    );
    let out = text(&o.stdout);
    assert!(
        out.contains(
            "included by core.git (.config/git/config) at position 3 of 3, only when gitdir:~/work/"
        ),
        "{out}"
    );
    assert!(
        out.contains(
            "1. ~/.config/external-overlays/git/base.inc -> core.git-defaults (module core)"
        ),
        "{out}"
    );
    assert!(
        out.contains("Rule: later include entries override earlier ones"),
        "{out}"
    );
}

#[test]
fn source_loading_skips_a_top_level_git_directory_and_refuses_the_rest() {
    let (home, _t, root) = bound("work-01.toml");
    let r = root.to_str().unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join(".git/config"), "[core]\n").unwrap();
    let o = run(home.path(), &["explain", "core.git", "--root", r]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));

    // A nested .git is a submodule or gitlink: refused.
    let nested = root.join("base/modules/core/.git");
    fs::write(&nested, "gitdir: elsewhere\n").unwrap();
    let o = run(home.path(), &["explain", "core.git", "--root", r]);
    assert_eq!(o.status.code(), Some(3));
    assert!(
        text(&o.stderr).contains("E_SOURCE_UNTRUSTED"),
        "{}",
        text(&o.stderr)
    );
    fs::remove_file(&nested).unwrap();

    // A FIFO is a special file: refused without opening it, so the command cannot hang.
    let fifo = root.join("base/modules/core/files/pipe");
    let mut mkfifo = Command::new("/usr/bin/mkfifo");
    mkfifo.arg(&fifo);
    assert!(finish_within(mkfifo, 10).status.success());
    let mut explain = Command::new(BIN);
    explain
        .env_clear()
        .arg("--home")
        .arg(home.path())
        .args(["explain", "core.git", "--root", r]);
    let o = finish_within(explain, 20);
    assert_eq!(o.status.code(), Some(3));
    assert!(
        text(&o.stderr).contains("E_SOURCE_UNTRUSTED"),
        "{}",
        text(&o.stderr)
    );
    fs::remove_file(&fifo).unwrap();

    // A symlink is recorded, not followed: one that leaves the module is a containment error.
    fs::remove_file(root.join("base/modules/core/files/gitconfig")).unwrap();
    std::os::unix::fs::symlink("/etc/hosts", root.join("base/modules/core/files/gitconfig"))
        .unwrap();
    let o = run(home.path(), &["explain", "core.git", "--root", r]);
    assert_eq!(o.status.code(), Some(3));
    assert!(
        text(&o.stderr).contains("E_SOURCE_ESCAPE"),
        "{}",
        text(&o.stderr)
    );
}

#[test]
fn explain_refusals_have_a_stable_code_and_exit() {
    let (home, _t, root) = bound("work-01.toml");
    let r = root.to_str().unwrap();
    // Usage errors.
    for args in [
        vec!["explain"],
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
    // A root that cannot be read means no report can be produced: exit 2, not a gate (exit 3).
    let o = run(
        home.path(),
        &["explain", "core.git", "--root", "/nonexistent-dfm-root"],
    );
    assert_eq!(o.status.code(), Some(2));
    assert!(
        text(&o.stderr).starts_with("error: E_REPORT_UNAVAILABLE"),
        "{}",
        text(&o.stderr)
    );
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
    assert!(text(&o.stdout).contains("explain <resource-or-target> [--root <dir>]"));
}

#[test]
fn a_path_with_a_bidirectional_control_is_shown_escaped_in_the_refusal() {
    let (home, _t, root) = bound("work-01.toml");
    let r = root.to_str().unwrap();
    // A FIFO whose name tries to reorder the terminal line that names it.
    let fifo = root.join("base/modules/core/files/evil\u{202e}gpj.txt");
    let mut mkfifo = Command::new("/usr/bin/mkfifo");
    mkfifo.arg(&fifo);
    assert!(finish_within(mkfifo, 10).status.success());
    let mut explain = Command::new(BIN);
    explain
        .env_clear()
        .arg("--home")
        .arg(home.path())
        .args(["explain", "core.git", "--root", r]);
    let o = finish_within(explain, 20);
    assert_eq!(o.status.code(), Some(3));
    let err = text(&o.stderr);
    assert!(err.contains("E_SOURCE_UNTRUSTED"), "{err}");
    assert!(err.contains("evil\\u{202e}gpj.txt"), "{err}");
    assert!(
        !err.contains('\u{202e}'),
        "the raw character reached the terminal: {err:?}"
    );
}

#[test]
fn argument_text_is_escaped_before_it_is_printed() {
    let home = TempDir::new("args");
    for arg in [
        "bad\u{202e}cmd",
        "bad\u{200b}cmd",
        "bad\u{2028}cmd",
        "bad\u{e0041}cmd",
    ] {
        let o = run(home.path(), &[arg]);
        assert_eq!(o.status.code(), Some(2), "{arg:?}");
        let err = text(&o.stderr);
        assert!(err.starts_with("error: E_USAGE"), "{err}");
        assert!(err.contains("\\u{"), "{err}");
        assert!(err.chars().all(|c| !c.is_control() || c == '\n'), "{err:?}");
        assert!(!err.contains(arg), "{err:?}");
    }
}

#[test]
fn a_non_utf8_argument_is_a_usage_error_not_a_panic() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let bad = OsString::from_vec(vec![b'x', 0xff, 0xfe]);
    for args in [
        vec![bad.clone()],
        vec![OsString::from("explain"), bad.clone()],
    ] {
        let o = Command::new(BIN).env_clear().args(&args).output().unwrap();
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        let err = text(&o.stderr);
        assert!(err.starts_with("error: E_USAGE argument "), "{err}");
        assert!(err.contains("is not valid UTF-8"), "{err}");
        assert!(!err.contains("panicked"), "{err}");
    }
}

#[test]
fn a_profile_for_another_architecture_is_refused_except_by_explain() {
    let home = TempDir::new("home");
    let (_t, root) = examples_copy();
    let p = root.join("profiles/personal-dev-01.toml");
    let body = fs::read_to_string(&p).unwrap();
    assert!(body.contains("architecture = \"arm64\""));
    let bind = run(
        home.path(),
        &[
            "bind",
            "--profile",
            p.to_str().unwrap(),
            "--machine-id",
            "m5-personal",
            "--root",
            root.to_str().unwrap(),
        ],
    );
    assert_eq!(bind.status.code(), Some(0), "{}", text(&bind.stderr));
    // The profile changes after the bind: bind itself refuses a profile for another machine.
    fs::write(&p, body.replace("\"arm64\"", "\"x86_64\"")).unwrap();
    let r = root.to_str().unwrap();
    for cmd in ["plan", "apply", "doctor"] {
        let o = run(home.path(), &[cmd, "--root", r]);
        let (out, err) = (text(&o.stdout), text(&o.stderr));
        assert!(
            out.contains("E_PLATFORM_MISMATCH") || err.contains("E_PLATFORM_MISMATCH"),
            "{cmd}: {out}{err}"
        );
        assert!(
            cmd == "doctor" || o.status.code() == Some(3),
            "{cmd}: {:?}",
            o.status
        );
    }
    let o = run(home.path(), &["explain", "core.git-defaults", "--root", r]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(
        out.contains("note: E_PLATFORM_MISMATCH the profile targets macos/x86_64"),
        "{out}"
    );
}

#[test]
fn bind_refuses_a_profile_for_another_architecture_and_writes_nothing() {
    let home = TempDir::new("home");
    let (_t, root) = examples_copy();
    let p = root.join("profiles/personal-dev-01.toml");
    let body = fs::read_to_string(&p).unwrap();
    fs::write(&p, body.replace("\"arm64\"", "\"x86_64\"")).unwrap();
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
    assert_eq!(o.status.code(), Some(3), "{}", text(&o.stderr));
    let err = text(&o.stderr);
    assert!(err.contains("E_PLATFORM_MISMATCH"), "{err}");
    assert!(
        fs::read_dir(home.path()).unwrap().next().is_none(),
        "bind wrote under HOME"
    );
}
