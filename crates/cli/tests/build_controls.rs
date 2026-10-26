//! Differential test: `scripts/bootstrap.sh check-controls` and `self-check --build-controls`
//! must give the same result on every shared fixture (docs/bootstrap.md).

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

    /// The refusing path: the first path-like word after the stable code (it has a `/` or an
    /// extension, so a bare version such as `9.9.9` is not one), with the fixture directories
    /// removed and a trailing colon trimmed. The two implementations word the rest of the line
    /// differently, and the script names no path for a toolchain digest mismatch.
    fn location(&self, roots: &[PathBuf]) -> Option<String> {
        let at = self.stderr.find("E_")?;
        let mut text = self.stderr[at..].to_string();
        for r in roots {
            let canonical = fs::canonicalize(r).unwrap_or_else(|_| r.clone());
            for p in [r, &canonical] {
                text = text.replace(&format!("{}/", p.display()), "");
            }
        }
        let is_path = |w: &&str| {
            w.contains('/')
                || w.trim_end_matches(':')
                    .rsplit_once('.')
                    .is_some_and(|(_, ext)| {
                        !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphabetic())
                    })
        };
        text.split_whitespace()
            .skip(1)
            .find(is_path)
            .map(|w| w.trim_end_matches(':').to_string())
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
        Command::new(env!("CARGO_BIN_EXE_dfm"))
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
    if extra.lines().any(|l| l == "toolchain=group-writable-file") {
        set_mode(&tc.join("lib/libstd.rlib"), 0o664);
    }
    if extra.lines().any(|l| l == "toolchain=world-writable-dir") {
        set_mode(&tc.join("lib"), 0o757);
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
    if !p.exists() {
        fs::create_dir_all(p).unwrap();
    }
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
}

/// Restores a directory mode on drop, so a panic cannot leave an unsearchable directory
/// behind that the temporary directory cannot remove.
struct RestoreMode(PathBuf);

impl Drop for RestoreMode {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        if self.0.exists() {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }
}

#[test]
fn fixture_toolchain_digest_is_pinned() {
    // Changing the digest algorithm changes every lock. Update this value deliberately.
    assert_eq!(
        toolchain_digest(),
        "3c8110d54cf8a1eb55956c01491677301c13522f094f26312703e7ffdf087f1c"
    );
}

/// Refusing fixtures where at least one implementation names no path of its own. The script
/// names none for a toolchain digest mismatch or a legacy toolchain pin, and each side names a
/// different set for the stage symlink and the two writable toolchain entries. Any other
/// refusing fixture must name a path on both sides, and the two must agree.
const WITHOUT_BOTH_PATHS: [&str; 7] = [
    "stage-rust-toolchain-legacy",
    "stage-rust-toolchain-legacy-case-variant",
    "stage-symlink",
    "toolchain-digest-differs-in-lock",
    "toolchain-group-writable-file",
    "toolchain-tampered",
    "toolchain-world-writable-dir",
];

/// Whether two refusing paths differ. For an unsearchable ancestor the script names that
/// directory (its `cd` fails) and the manager names the config file it cannot stat below it,
/// so there one path may lie inside the other, on a path component boundary.
fn paths_differ(a: &str, b: &str, nested_ok: bool) -> bool {
    let inside = |p: &str, dir: &str| p == dir || p.starts_with(&format!("{dir}/"));
    if nested_ok {
        !(inside(a, b) || inside(b, a))
    } else {
        a != b
    }
}

#[test]
fn nested_paths_are_accepted_only_on_a_component_boundary() {
    assert!(!paths_differ(".claude", ".claude/settings.json", true));
    assert!(!paths_differ(".claude/settings.json", ".claude", true));
    assert!(paths_differ(".claude", ".claude2", true));
    assert!(paths_differ(".claude", ".claude/settings.json", false));
}

#[test]
fn shell_checker_and_manager_agree_on_every_fixture() {
    let mut cases: Vec<PathBuf> = fs::read_dir(fixtures().join("cases"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    cases.sort();
    // 114 fixtures today; the margin allows a few removals but not a lost group.
    assert!(cases.len() >= 110, "fixtures went missing");
    let mut failures = Vec::new();
    let mut compared = 0;
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
        let _restore = RestoreMode(home.join(".cache/dotfiles-manager"));
        let (m, s) = (run_manager(&home, &stage), run_script(&home, &stage));
        let roots = [home.clone(), stage.clone()];
        let nested_ok = fs::read_to_string(case.join("expect"))
            .unwrap()
            .lines()
            .any(|l| l == "unsearchable-ancestor");
        let expected_exit = if want == "ok" { 0 } else { 3 };
        let path_mismatch = want != "ok"
            && match (m.location(&roots), s.location(&roots)) {
                (Some(a), Some(b)) => {
                    compared += 1;
                    paths_differ(&a, &b, nested_ok)
                }
                _ => !WITHOUT_BOTH_PATHS.contains(&name.as_str()),
            };
        if m.verdict() != want
            || s.verdict() != want
            || m.code != expected_exit
            || s.code != expected_exit
            || path_mismatch
        {
            failures.push(format!(
                "{name}: want {want}/{expected_exit}, manager {}/{} ({}), script {}/{} ({}), paths {:?} and {:?}",
                m.verdict(),
                m.code,
                m.stderr.trim(),
                s.verdict(),
                s.code,
                s.stderr.trim(),
                m.location(&roots),
                s.location(&roots)
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
    // 97 of 104 refusing fixtures compare two paths today; a lost extraction would show here.
    assert!(compared >= 90, "only {compared} fixtures compared paths");
}

#[test]
fn unreadable_directory_in_the_staged_tree_is_refused_by_both_checkers() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new("unreadable");
    let (home, stage) = materialize(&fixtures().join("cases/ok-base"), tmp.path());
    let hidden = stage.join("crates/b");
    let _restore = RestoreMode(hidden.clone());
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

/// The manager refuses a toolchain location whose ancestors below HOME another user could swap:
/// a link, or a directory open to group or others. The shell checker enforces the same rule in
/// `install` (`check_data_tree`), tested in tests/bootstrap.rs.
#[test]
fn a_linked_or_open_toolchain_ancestor_is_refused_by_the_manager() {
    use std::os::unix::fs::PermissionsExt;
    type Damage = fn(&Path);
    let cases: [(&str, Damage); 4] = [
        ("share-open", |h| set_mode(&h.join(".local/share"), 0o775)),
        ("toolchains-open", |h| {
            set_mode(&h.join(".local/share/dotfiles-manager/toolchains"), 0o757)
        }),
        ("local-link", |h| {
            let real = h.join("real-local");
            fs::rename(h.join(".local"), &real).unwrap();
            std::os::unix::fs::symlink(&real, h.join(".local")).unwrap();
        }),
        ("manager-link", |h| {
            let m = h.join(".local/share/dotfiles-manager");
            fs::rename(&m, h.join("real-manager")).unwrap();
            std::os::unix::fs::symlink(h.join("real-manager"), &m).unwrap();
        }),
    ];
    for (name, damage) in cases {
        let tmp = TempDir::new(name);
        let (home, stage) = materialize(&fixtures().join("cases/ok-base"), tmp.path());
        let ok = run_manager(&home, &stage);
        assert_eq!(ok.code, 0, "{name} control: {}", ok.stderr);
        let _restore = RestoreMode(home.join(".local/share"));
        damage(&home);
        let m = run_manager(&home, &stage);
        fs::set_permissions(home.join(".local/share"), fs::Permissions::from_mode(0o755)).ok();
        assert_eq!(
            (m.code, m.verdict()),
            (3, "E_TOOLCHAIN_MISMATCH".into()),
            "{name}: {}",
            m.stderr
        );
    }
}

fn shell_digest(home: &Path, dir: &Path) -> Output {
    Command::new("/bin/sh")
        .arg(repo_root().join("scripts/bootstrap.sh"))
        .args(["toolchain-digest", "--dir", dir.to_str().unwrap()])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("DFM_BOOTSTRAP_TEST", "1")
        .env("DFM_TEST_HOME", home)
        .output()
        .unwrap()
}

#[test]
fn a_toolchain_tree_without_a_regular_file_is_refused_by_both_digests() {
    let tmp = TempDir::new("no-file");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let empty = tmp.path().join("empty");
    fs::create_dir_all(empty.join("bin")).unwrap();
    let links_only = tmp.path().join("links");
    fs::create_dir_all(links_only.join("bin")).unwrap();
    std::os::unix::fs::symlink("../elsewhere", links_only.join("bin/a")).unwrap();
    for tree in [&empty, &links_only] {
        let rust = dfm_cli::toolchain_tree_digest(tree);
        assert!(rust.is_err(), "{}: Rust digest {rust:?}", tree.display());
        let sh = shell_digest(&home, tree);
        assert_eq!(sh.status.code(), Some(3), "{}", tree.display());
        assert!(sh.stdout.is_empty());
    }
}
