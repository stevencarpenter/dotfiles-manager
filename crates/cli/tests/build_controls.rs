//! Differential test: `scripts/bootstrap.sh check-controls` and `self-check --build-controls`
//! must give the same result on every shared fixture (docs/architecture.md section 14, open
//! risk "Bootstrap checker ... matches self-check --build-controls").

mod common;

use common::{TempDir, copy_tree, repo_root};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const PIN: &str = "9.9.9";

fn fixtures() -> PathBuf {
    repo_root().join("tests/fixtures/build-controls")
}

/// Digest of the fixture toolchain after materialization (symlinks created, `.in` stripped).
fn toolchain_digest() -> String {
    let tmp = TempDir::new("digest");
    copy_tree(&fixtures().join("toolchain"), &tmp.path().join("t"));
    finish(&tmp.path().join("t"), "");
    dfm_cli::toolchain_tree_digest(&tmp.path().join("t")).unwrap()
}

/// Replaces `@TOOLCHAIN_TREE@` and `@CR@` (git may normalise a raw CR), strips the `.in` suffix and turns `*.symlink` files into
/// symlinks (the repository itself must contain none) below `dir`.
fn finish(dir: &Path, digest: &str) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_symlink() {
            continue;
        }
        if p.is_dir() {
            finish(&p, digest);
        } else if p.extension().is_some_and(|x| x == "symlink") {
            let target = fs::read_to_string(&p).unwrap();
            fs::remove_file(&p).unwrap();
            std::os::unix::fs::symlink(target, p.with_extension("")).unwrap();
        } else if p.extension().is_some_and(|x| x == "in") {
            let text = fs::read(&p).unwrap();
            let text = String::from_utf8_lossy(&text)
                .replace("@TOOLCHAIN_TREE@", digest)
                .replace("@CR@", "\r")
                .replace("@NUL@", "\0");
            fs::write(p.with_extension(""), text.as_bytes()).unwrap();
            fs::remove_file(&p).unwrap();
        }
    }
}

struct Run {
    code: i32,
    stderr: String,
}

impl Run {
    fn from(o: Output) -> Run {
        Run {
            code: o.status.code().unwrap(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }

    /// `ok` or the first stable error code.
    fn verdict(&self) -> String {
        if self.code == 0 {
            return "ok".into();
        }
        // A failure without a stable code is itself a difference worth reporting.
        let Some(at) = self.stderr.find("E_") else {
            return format!("NO_CODE(exit {})", self.code);
        };
        self.stderr[at..]
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || *c == '_')
            .collect()
    }
}

fn run_manager(home: &Path, stage: &Path) -> Run {
    Run::from(
        Command::new(env!("CARGO_BIN_EXE_dotfiles-manager"))
            .env_clear()
            .args([
                "--home",
                home.to_str().unwrap(),
                "self-check",
                "--build-controls",
                stage.to_str().unwrap(),
            ])
            .output()
            .unwrap(),
    )
}

fn run_script(home: &Path, stage: &Path) -> Run {
    Run::from(
        Command::new("/bin/sh")
            .arg(repo_root().join("scripts/bootstrap.sh"))
            .args(["check-controls", "--source", stage.to_str().unwrap()])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", home)
            .env("DFM_BOOTSTRAP_TEST", "1")
            .env("DFM_TEST_HOME", home)
            .output()
            .unwrap(),
    )
}

fn materialize(case: &Path, tmp: &Path) -> (PathBuf, PathBuf) {
    let (home, stage) = (tmp.join("home"), tmp.join("stage"));
    copy_tree(&fixtures().join("base"), &stage);
    let overlay = |name: &str, to: &Path| {
        if case.join(name).is_dir() {
            copy_tree(&case.join(name), to);
        }
    };
    fs::create_dir_all(&home).unwrap();
    let delete = case.join("delete");
    if delete.exists() {
        for line in fs::read_to_string(delete).unwrap().lines() {
            fs::remove_file(stage.join(line)).unwrap();
        }
    }
    overlay("tree", &stage);
    overlay("home", &home);
    let extra = fs::read_to_string(case.join("expect")).unwrap();
    let tc = home
        .join(".local/share/dotfiles-manager/toolchains")
        .join(PIN);
    if !extra.lines().any(|l| l == "toolchain=absent") {
        copy_tree(&fixtures().join("toolchain"), &tc);
    }
    if tc.exists() {
        finish(&tc, "");
    }
    if extra.lines().any(|l| l == "toolchain=tampered") {
        fs::write(tc.join("lib/libstd.rlib"), "tampered\n").unwrap();
    }
    let digest = toolchain_digest();
    finish(&stage, &digest);
    finish(&home, &digest);
    if extra.lines().any(|l| l == "unsearchable-ancestor") {
        // The build directory's parent cannot be searched, so nothing below it can be checked.
        set_mode(&home.join(".cache/dotfiles-manager"), 0o600);
    }
    (home, stage)
}

fn set_mode(p: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(p).ok();
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn fixture_toolchain_digest_is_pinned() {
    // Changing the digest algorithm changes every lock. Update this value deliberately.
    assert_eq!(
        toolchain_digest(),
        "3c8110d54cf8a1eb55956c01491677301c13522f094f26312703e7ffdf087f1c"
    );
}

#[test]
fn shell_checker_and_manager_agree_on_every_fixture() {
    let mut cases: Vec<PathBuf> = fs::read_dir(fixtures().join("cases"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    cases.sort();
    assert!(cases.len() >= 60, "fixtures went missing");
    let mut failures = Vec::new();
    for case in &cases {
        let name = case.file_name().unwrap().to_string_lossy().into_owned();
        let want = fs::read_to_string(case.join("expect"))
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        let tmp = TempDir::new("controls");
        let (home, stage) = materialize(case, tmp.path());
        let (m, s) = (run_manager(&home, &stage), run_script(&home, &stage));
        set_mode(&home.join(".cache/dotfiles-manager"), 0o755);
        let expected_exit = if want == "ok" { 0 } else { 3 };
        if m.verdict() != want
            || s.verdict() != want
            || m.code != expected_exit
            || s.code != expected_exit
        {
            failures.push(format!(
                "{name}: want {want}/{expected_exit}, manager {}/{} ({}), script {}/{} ({})",
                m.verdict(),
                m.code,
                m.stderr.trim(),
                s.verdict(),
                s.code,
                s.stderr.trim()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} fixtures differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[test]
fn unreadable_directory_in_the_staged_tree_is_refused_by_both_checkers() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new("unreadable");
    let (home, stage) = materialize(&fixtures().join("cases/ok-base"), tmp.path());
    let hidden = stage.join("crates/b");
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
    let (m, s) = (run_manager(&home, &stage), run_script(&home, &stage));
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        (m.code, m.verdict()),
        (3, "E_SOURCE_UNTRUSTED".into()),
        "manager: {}",
        m.stderr
    );
    assert_eq!(
        (s.code, s.verdict()),
        (3, "E_SOURCE_UNTRUSTED".into()),
        "script: {}",
        s.stderr
    );
}
