//! `scripts/bootstrap.sh`: refusals before mutation, and a full isolated install,
//! invocation and clean removal with a real build.

mod common;

use common::{TempDir, repo_root};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn script(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut c = Command::new("/bin/sh");
    c.arg(repo_root().join("scripts/bootstrap.sh"))
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home);
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().unwrap()
}

/// Test mode makes the script take the home from DFM_TEST_HOME instead of dscl.
fn script_test_mode(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut e = vec![
        ("DFM_BOOTSTRAP_TEST", "1"),
        ("DFM_TEST_HOME", home.to_str().unwrap()),
    ];
    e.extend_from_slice(env);
    script(home, args, &e)
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("/usr/bin/git")
        .current_dir(dir)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/var/empty")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .output()
        .unwrap();
    assert!(o.status.success(), "git {args:?}: {}", err(&o));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn entries_below(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).into_iter().flatten() {
        let p = e.unwrap().path();
        out.push(p.clone());
        if p.is_dir() && !p.is_symlink() {
            out.extend(entries_below(&p));
        }
    }
    out
}

/// A tiny committed source with only a lock file. Enough for the refusals before any build.
fn tiny_source(tmp: &Path, pin: &str) -> (PathBuf, String) {
    let src = tmp.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        src.join("dotfiles-manager.lock"),
        format!(
            "schema = 1\n\n[manager]\ntoolchain = \"{pin}\"\ntoolchain_tree_sha256 = \"{}\"\n",
            "0".repeat(64)
        ),
    )
    .unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-q", "-m", "lock"]);
    let sha = git(&src, &["rev-parse", "HEAD"]);
    (src, sha)
}

#[test]
fn unsupported_os_or_architecture_is_refused_before_mutation() {
    let tmp = TempDir::new("script-platform");
    let home = tmp.path().join("home");
    fs::create_dir(&home).unwrap();
    let (src, sha) = tiny_source(tmp.path(), "9.9.9");
    for (os, arch) in [
        ("Linux", "arm64"),
        ("Darwin", "x86_64"),
        ("Linux", "x86_64"),
    ] {
        let o = script_test_mode(
            &home,
            &[
                "install",
                "--source",
                src.to_str().unwrap(),
                "--commit",
                &sha,
            ],
            &[("DFM_TEST_OS", os), ("DFM_TEST_ARCH", arch)],
        );
        assert_eq!(o.status.code(), Some(3), "{os}/{arch}");
        assert!(err(&o).contains("E_PLATFORM_UNSUPPORTED"), "{}", err(&o));
    }
    assert!(
        entries_below(&home).is_empty(),
        "refusal must precede any write"
    );
}

#[test]
fn a_home_with_whitespace_is_refused_by_the_script() {
    let tmp = TempDir::new("script-space");
    let home = tmp.path().join("a b");
    fs::create_dir(&home).unwrap();
    let o = script_test_mode(&home, &["uninstall"], &[]);
    assert_eq!(o.status.code(), Some(3));
    assert!(
        err(&o).contains("E_HOME_MISMATCH") && err(&o).contains("whitespace"),
        "{}",
        err(&o)
    );
    assert!(entries_below(&home).is_empty());
}

#[test]
fn toolchain_digest_refuses_entries_writable_by_group_or_others() {
    let tmp = TempDir::new("writable");
    let home = tmp.path().join("home");
    let tc = tmp.path().join("tc");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(tc.join("bin")).unwrap();
    fs::write(tc.join("bin/a"), "a").unwrap();
    let args = ["toolchain-digest", "--dir", tc.to_str().unwrap()];
    assert_eq!(script_test_mode(&home, &args, &[]).status.code(), Some(0));
    for (path, mode) in [
        ("bin/a", 0o664),
        ("bin/a", 0o646),
        ("bin", 0o775),
        ("bin", 0o757),
    ] {
        let p = tc.join(path);
        let old = fs::metadata(&p).unwrap().permissions().mode() & 0o7777;
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
        let o = script_test_mode(&home, &args, &[]);
        assert_eq!(o.status.code(), Some(3), "{path} at {mode:o}");
        assert!(o.stdout.is_empty());
        fs::set_permissions(&p, fs::Permissions::from_mode(old)).unwrap();
    }
}

#[test]
fn toolchain_digest_refuses_file_names_with_backslashes() {
    let tmp = TempDir::new("backslash");
    let home = tmp.path().join("home");
    let tc = tmp.path().join("tc");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&tc).unwrap();
    fs::write(tc.join("ok"), "a").unwrap();
    let args = ["toolchain-digest", "--dir", tc.to_str().unwrap()];
    assert_eq!(script_test_mode(&home, &args, &[]).status.code(), Some(0));
    fs::write(tc.join("a\\b"), "b").unwrap();
    let o = script_test_mode(&home, &args, &[]);
    assert_eq!(
        o.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    assert!(o.stdout.is_empty());
}

#[test]
fn home_must_match_the_password_database() {
    let tmp = TempDir::new("script-home");
    let o = script(tmp.path(), &["uninstall"], &[]);
    assert_eq!(o.status.code(), Some(3));
    assert!(err(&o).contains("E_HOME_MISMATCH"), "{}", err(&o));
    assert!(entries_below(tmp.path()).is_empty());
}

#[test]
fn missing_toolchain_wrong_commit_and_hostile_repo_config_are_refused_before_mutation() {
    let tmp = TempDir::new("script-prereq");
    let home = tmp.path().join("home");
    fs::create_dir(&home).unwrap();
    let (src, sha) = tiny_source(tmp.path(), "9.9.9");
    let s = src.to_str().unwrap();

    let o = script_test_mode(&home, &["install", "--source", s, "--commit", &sha], &[]);
    assert_eq!(o.status.code(), Some(3));
    assert!(
        err(&o).contains("E_TOOLCHAIN_MISMATCH") && err(&o).contains("9.9.9"),
        "{}",
        err(&o)
    );

    let o = script_test_mode(
        &home,
        &["install", "--source", s, "--commit", &"a".repeat(40)],
        &[],
    );
    assert_eq!(o.status.code(), Some(3));
    assert!(err(&o).contains("E_SOURCE_UNTRUSTED"), "{}", err(&o));

    // Repository-local configuration must never run code: a clean filter would run on `git status`.
    let marker = tmp.path().join("filter-ran");
    fs::write(src.join(".gitattributes"), "* filter=evil\n").unwrap();
    fs::write(
        src.join("dotfiles-manager.lock"),
        "changed so git must re-hash it\n",
    )
    .unwrap();
    let mut cfg = fs::OpenOptions::new()
        .append(true)
        .open(src.join(".git/config"))
        .unwrap();
    std::io::Write::write_all(
        &mut cfg,
        format!("[filter \"evil\"]\n\tclean = touch {}\n", marker.display()).as_bytes(),
    )
    .unwrap();
    let o = script_test_mode(&home, &["install", "--source", s, "--commit", &sha], &[]);
    assert!(!marker.exists(), "repository configuration executed code");
    assert_eq!(o.status.code(), Some(3));
    assert!(err(&o).contains("E_TOOLCHAIN_MISMATCH"), "{}", err(&o));

    for bad in [vec!["install"], vec!["bogus"], vec!["install", "--nope"]] {
        assert_eq!(
            script_test_mode(&home, &bad, &[]).status.code(),
            Some(2),
            "{bad:?}"
        );
    }
    assert!(
        entries_below(&home).is_empty(),
        "refusals must precede any write"
    );
}

#[test]
fn toolchain_digest_fails_closed_instead_of_dropping_unreadable_entries() {
    let tmp = TempDir::new("digest-closed");
    let home = tmp.path().join("home");
    let tc = tmp.path().join("tc");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(tc.join("bin")).unwrap();
    fs::write(tc.join("bin/a"), "a").unwrap();
    fs::write(tc.join("bin/b"), "b").unwrap();
    let ok = script_test_mode(
        &home,
        &["toolchain-digest", "--dir", tc.to_str().unwrap()],
        &[],
    );
    assert_eq!(ok.status.code(), Some(0));
    fs::set_permissions(tc.join("bin/b"), fs::Permissions::from_mode(0o000)).unwrap();
    let o = script_test_mode(
        &home,
        &["toolchain-digest", "--dir", tc.to_str().unwrap()],
        &[],
    );
    assert_eq!(
        o.status.code(),
        Some(3),
        "a partial digest was printed: {}",
        String::from_utf8_lossy(&o.stdout)
    );
    assert!(o.stdout.is_empty());
    let empty = tmp.path().join("empty");
    fs::create_dir(&empty).unwrap();
    let o = script_test_mode(
        &home,
        &["toolchain-digest", "--dir", empty.to_str().unwrap()],
        &[],
    );
    assert_eq!(
        o.status.code(),
        Some(3),
        "an empty toolchain must not produce a digest"
    );
}

#[test]
fn replace_refs_cannot_swap_the_built_commit() {
    let tmp = TempDir::new("replace");
    let home = tmp.path().join("home");
    fs::create_dir(&home).unwrap();
    let (src, good) = tiny_source(tmp.path(), "9.9.9");
    git(&src, &["checkout", "-q", "-b", "evil"]);
    fs::write(
        src.join("dotfiles-manager.lock"),
        "schema = 1\n\n[manager]\ntoolchain = \"8.8.8\"\n",
    )
    .unwrap();
    git(&src, &["commit", "-q", "-a", "-m", "evil"]);
    let evil = git(&src, &["rev-parse", "HEAD"]);
    git(&src, &["checkout", "-q", "main"]);
    git(&src, &["replace", &good, &evil]);
    let o = script_test_mode(
        &home,
        &[
            "install",
            "--source",
            src.to_str().unwrap(),
            "--commit",
            &good,
        ],
        &[],
    );
    assert_eq!(o.status.code(), Some(3));
    assert!(
        err(&o).contains("9.9.9") && !err(&o).contains("8.8.8"),
        "replacement ref was honoured: {}",
        err(&o)
    );
}

#[test]
fn caller_environment_cannot_influence_the_script() {
    let tmp = TempDir::new("env");
    let home = tmp.path().join("home");
    let tc = tmp.path().join("tc");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(tc.join("bin")).unwrap();
    fs::write(tc.join("bin/a"), "a").unwrap();
    let digest = |env: &[(&str, &str)], dir: &str, cwd: &Path| {
        let mut c = Command::new("/bin/sh");
        c.arg(repo_root().join("scripts/bootstrap.sh"))
            .args(["toolchain-digest", "--dir", dir])
            .current_dir(cwd)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("DFM_BOOTSTRAP_TEST", "1")
            .env("DFM_TEST_HOME", &home);
        for (k, v) in env {
            c.env(k, v);
        }
        String::from_utf8_lossy(&c.output().unwrap().stdout)
            .trim()
            .to_string()
    };
    let clean = digest(&[], tc.to_str().unwrap(), tmp.path());
    assert_eq!(clean.len(), 64);
    // shasum is perl: a forged module must not run. An exported function must not shadow awk.
    let forge = tmp.path().join("forge");
    fs::create_dir(&forge).unwrap();
    fs::write(
        forge.join("Forge.pm"),
        format!(
            "package Forge; BEGIN {{ print \"{}  forged\\n\"; exit 0 }} 1;\n",
            "f".repeat(64)
        ),
    )
    .unwrap();
    let hostile = [
        ("PERL5LIB", forge.to_str().unwrap()),
        ("PERL5OPT", "-MForge"),
        ("BASH_FUNC_awk%%", "() { echo forged; }"),
        (
            "BASH_FUNC_cut%%",
            "() { echo ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff; }",
        ),
        ("BASH_ENV", "/nonexistent"),
        ("ENV", "/nonexistent"),
        // The old sentinel: exporting it must not skip the scrub.
        ("DFM_BS_CLEAN", "1"),
    ];
    assert_eq!(
        digest(&hostile, tc.to_str().unwrap(), tmp.path()),
        clean,
        "caller environment changed the digest"
    );
    // A caller-supplied marker must not skip the scrub either: the child refuses a dirty
    // environment instead of trusting the marker.
    let with_marker = |env: &[(&str, &str)]| {
        let mut c = Command::new("/bin/sh");
        c.arg(repo_root().join("scripts/bootstrap.sh"))
            .args([
                "--dfm-scrubbed",
                "toolchain-digest",
                "--dir",
                tc.to_str().unwrap(),
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("DFM_BOOTSTRAP_TEST", "1")
            .env("DFM_TEST_HOME", &home);
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    };
    for hostile_var in [
        ("PERL5OPT", "-MForge"),
        ("BASH_FUNC_cut%%", "() { echo forged; }"),
        ("DFM_BS_CLEAN", "1"),
        ("CDPATH", "/tmp"),
    ] {
        let o = with_marker(&[("PERL5LIB", forge.to_str().unwrap()), hostile_var]);
        assert_eq!(
            o.status.code(),
            Some(3),
            "{hostile_var:?}: {}",
            String::from_utf8_lossy(&o.stdout)
        );
        assert!(
            o.stdout.is_empty() && err(&o).contains("E_ENV_UNEXPECTED"),
            "{}",
            err(&o)
        );
    }
    // A relative --dir with CDPATH pointing at a decoy must still resolve against the cwd.
    let decoy = tmp.path().join("decoy");
    fs::create_dir_all(decoy.join("tc/bin")).unwrap();
    fs::write(decoy.join("tc/bin/a"), "different").unwrap();
    assert_eq!(
        digest(&[("CDPATH", decoy.to_str().unwrap())], "tc", tmp.path()),
        clean
    );
}

#[test]
fn test_variables_are_forwarded_only_in_test_mode() {
    let tmp = TempDir::new("fwd");
    let home = tmp.path().join("home");
    fs::create_dir(&home).unwrap();
    // Without DFM_BOOTSTRAP_TEST the DFM_TEST_* values must be dropped, so the real
    // password-database home is used and a temporary HOME is a mismatch.
    let o = script(
        &home,
        &["uninstall"],
        &[("DFM_TEST_HOME", home.to_str().unwrap())],
    );
    assert_eq!(o.status.code(), Some(3));
    assert!(err(&o).contains("E_HOME_MISMATCH"), "{}", err(&o));
}

#[test]
fn noexec_run_is_detectable_by_the_missing_final_line() {
    // SHELLOPTS=noexec makes /bin/sh skip the file; no script can prevent that. Callers must
    // look for the success line, which a skipped run never prints.
    let tmp = TempDir::new("noexec");
    let home = tmp.path().join("home");
    fs::create_dir(&home).unwrap();
    let o = script_test_mode(&home, &["uninstall"], &[("SHELLOPTS", "noexec")]);
    assert!(!String::from_utf8_lossy(&o.stdout).contains("removed the installed manager"));
}

#[test]
fn only_a_full_lowercase_object_id_is_accepted_as_the_pin() {
    let tmp = TempDir::new("pin");
    let home = tmp.path().join("home");
    fs::create_dir(&home).unwrap();
    let (src, good) = tiny_source(tmp.path(), "9.9.9");
    let s = src.to_str().unwrap();
    let marker = tmp.path().join("upload-pack-ran");
    let bad = [
        good[..12].to_string(),
        good.to_uppercase(),
        "main".into(),
        "HEAD".into(),
        "HEAD~0".into(),
        format!("{good}^{{}}"),
        format!("{good}~0"),
        "@{0}".into(),
        "@".into(),
        format!("--upload-pack=touch {}", marker.display()),
        "-h".into(),
        format!(" {good}"),
        format!("{good} "),
        format!("{good}\n"),
        good[..39].to_string(),
        format!("{good}0"),
        "g".repeat(40),
        "0".repeat(64),
        format!("{good}{}", &good[..24]),
    ];
    for commit in &bad {
        let o = script_test_mode(&home, &["install", "--source", s, "--commit", commit], &[]);
        assert_eq!(o.status.code(), Some(3), "{commit:?}: {}", err(&o));
        assert!(
            err(&o).contains("E_SOURCE_UNTRUSTED"),
            "{commit:?}: {}",
            err(&o)
        );
    }
    assert!(!marker.exists());
    assert!(
        entries_below(&home).is_empty(),
        "refusals must precede any write"
    );
}

#[test]
fn a_ref_named_like_the_pin_cannot_redirect_the_build() {
    let tmp = TempDir::new("pin-ref");
    let home = tmp.path().join("home");
    fs::create_dir(&home).unwrap();
    let (src, good) = tiny_source(tmp.path(), "9.9.9");
    git(&src, &["checkout", "-q", "-b", "other"]);
    fs::write(
        src.join("dotfiles-manager.lock"),
        "schema = 1\n\n[manager]\ntoolchain = \"8.8.8\"\n",
    )
    .unwrap();
    git(&src, &["commit", "-q", "-a", "-m", "other"]);
    let other = git(&src, &["rev-parse", "HEAD"]);
    // A branch whose name is the other commit's id but which points at the good commit.
    git(&src, &["branch", "-f", &other, &good]);
    let o = script_test_mode(
        &home,
        &[
            "install",
            "--source",
            src.to_str().unwrap(),
            "--commit",
            &other,
        ],
        &[],
    );
    assert_eq!(o.status.code(), Some(3));
    assert!(
        err(&o).contains("8.8.8") && !err(&o).contains("9.9.9"),
        "the ref redirected the pin: {}",
        err(&o)
    );
}

#[test]
fn a_64_digit_pin_needs_a_sha256_repository() {
    let tmp = TempDir::new("pin-sha256");
    let home = tmp.path().join("home");
    fs::create_dir(&home).unwrap();
    let (src, _) = tiny_source(tmp.path(), "9.9.9");
    let o = script_test_mode(
        &home,
        &[
            "install",
            "--source",
            src.to_str().unwrap(),
            "--commit",
            &"a".repeat(64),
        ],
        &[],
    );
    assert_eq!(o.status.code(), Some(3));
    assert!(
        err(&o).contains("E_SOURCE_UNTRUSTED") && err(&o).contains("object format"),
        "{}",
        err(&o)
    );
}

fn clone_path(from: &Path, to: &Path) {
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    let fast = Command::new("/bin/cp")
        .arg("-cR")
        .arg(from)
        .arg(to)
        .status()
        .unwrap()
        .success();
    if !fast {
        assert!(
            Command::new("/bin/cp")
                .arg("-R")
                .arg(from)
                .arg(to)
                .status()
                .unwrap()
                .success()
        );
    }
}

/// The smallest part of the host toolchain that can build this workspace.
fn minimal_toolchain(dest: &Path) -> String {
    let rustc = |arg: &str| {
        let o = Command::new("rustc")
            .current_dir(repo_root())
            .arg(arg)
            .output()
            .unwrap();
        assert!(o.status.success(), "rustc {arg} failed");
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    let version = rustc("-V").split_whitespace().nth(1).unwrap().to_string();
    let sysroot = PathBuf::from(rustc("--print=sysroot"));
    for rel in [
        "bin/cargo",
        "bin/rustc",
        "lib/libLLVM.dylib",
        "lib/rustlib/aarch64-apple-darwin/lib",
        "lib/rustlib/aarch64-apple-darwin/bin",
    ] {
        clone_path(&sysroot.join(rel), &dest.join(rel));
    }
    for e in fs::read_dir(sysroot.join("lib")).unwrap() {
        let e = e.unwrap();
        if e.file_name()
            .to_string_lossy()
            .starts_with("librustc_driver")
        {
            clone_path(&e.path(), &dest.join("lib").join(e.file_name()));
        }
    }
    version
}

/// Copies the repository working tree, skipping build output and tool caches.
fn copy_repo(from: &Path, to: &Path) {
    const SKIP: [&str; 8] = [
        ".git",
        "target",
        ".venv",
        ".ruff_cache",
        "__pycache__",
        ".lavish",
        ".idea",
        ".pi",
    ];
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let name = e.file_name();
        let ty = e.file_type().unwrap();
        if SKIP.iter().any(|s| name == *s) || ty.is_symlink() {
            continue;
        }
        if ty.is_dir() {
            copy_repo(&e.path(), &to.join(&name));
        } else {
            fs::copy(e.path(), to.join(&name)).unwrap();
        }
    }
}

/// Working-tree copy of this repository, committed, plus the lock a reviewer would write.
fn full_source(tmp: &Path, pin: &str, digest: &str) -> (PathBuf, String) {
    let src = tmp.join("src");
    copy_repo(&repo_root(), &src);
    fs::write(
        src.join("rust-toolchain.toml"),
        format!("[toolchain]\nchannel = \"{pin}\"\n"),
    )
    .unwrap();
    fs::write(
        src.join("dotfiles-manager.lock"),
        format!("schema = 1\n\n[manager]\nversion = \"0.1.0\"\ntoolchain = \"{pin}\"\ntoolchain_tree_sha256 = \"{digest}\"\n"),
    )
    .unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-q", "-m", "source"]);
    let sha = git(&src, &["rev-parse", "HEAD"]);
    (src, sha)
}

/// A temporary HOME with the pinned toolchain placed and a committed copy of this repository.
struct Prepared {
    tmp: TempDir,
    home: PathBuf,
    data: PathBuf,
    src: PathBuf,
    sha: String,
    tc: PathBuf,
}

fn prepare(label: &str) -> Prepared {
    let tmp = TempDir::new(label);
    let home = tmp.path().join("home");
    let data = home.join(".local/share/dotfiles-manager");
    fs::create_dir_all(&home).unwrap();
    // Owner step: place the exact pinned toolchain and record its tree digest in the lock.
    let tc_tmp = tmp.path().join("tc");
    let pin = minimal_toolchain(&tc_tmp);
    let tc = data.join("toolchains").join(&pin);
    fs::create_dir_all(tc.parent().unwrap()).unwrap();
    fs::rename(&tc_tmp, &tc).unwrap();
    let digest = dfm_cli::toolchain_tree_digest(&tc).unwrap();
    let shell_digest = script_test_mode(
        &home,
        &["toolchain-digest", "--dir", tc.to_str().unwrap()],
        &[],
    );
    assert_eq!(
        String::from_utf8_lossy(&shell_digest.stdout).trim(),
        digest,
        "shell and Rust toolchain digests differ"
    );
    let (src, sha) = full_source(tmp.path(), &pin, &digest);
    Prepared {
        tmp,
        home,
        data,
        src,
        sha,
        tc,
    }
}

fn install(p: &Prepared, sha: &str, env: &[(&str, &str)]) -> Output {
    script_test_mode(
        &p.home,
        &[
            "install",
            "--source",
            p.src.to_str().unwrap(),
            "--commit",
            sha,
        ],
        env,
    )
}

/// Commits a trivial change so the next install has a different commit (and version directory).
fn next_commit(p: &Prepared, note: &str) -> String {
    fs::write(p.src.join("NOTE"), note).unwrap();
    git(&p.src, &["add", "-A"]);
    git(&p.src, &["commit", "-q", "-m", note]);
    git(&p.src, &["rev-parse", "HEAD"])
}

#[test]
#[ignore = "needs crates.io"]
fn isolated_install_invocation_and_clean_removal() {
    let p = prepare("install");
    let (home, data, sha, tc) = (&p.home, &p.data, &p.sha, &p.tc);

    // A caller environment that tries to redirect the build: none of it may be used.
    let planted = p.tmp.path().join("planted");
    let marker = p.tmp.path().join("planted-was-invoked");
    fs::create_dir(&planted).unwrap();
    for tool in ["cargo", "rustc", "rustup", "cc"] {
        let f = planted.join(tool);
        fs::write(
            &f,
            format!("#!/bin/sh\necho {tool} >> {}\nexit 1\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o755)).unwrap();
    }
    // A read-only build directory left by an earlier run must not block the next one.
    let stale = home.join(".cache/dotfiles-manager/build/stage/sub");
    fs::create_dir_all(&stale).unwrap();
    fs::write(stale.join("x"), "x").unwrap();
    fs::set_permissions(&stale, fs::Permissions::from_mode(0o500)).unwrap();
    let path = format!("{}:/usr/bin:/bin", planted.display());
    let o = install(
        &p,
        sha,
        &[
            ("PATH", &path),
            ("RUSTUP_TOOLCHAIN", "planted"),
            ("RUSTC", planted.join("rustc").to_str().unwrap()),
            ("CARGO_HOME", "/nonexistent-cargo-home"),
            ("RUSTFLAGS", "--cfg planted"),
        ],
    );
    assert_eq!(
        o.status.code(),
        Some(0),
        "install failed:\n{}\n{}",
        String::from_utf8_lossy(&o.stdout),
        err(&o)
    );
    assert!(String::from_utf8_lossy(&o.stdout).contains("bootstrap: done"));
    assert!(
        !marker.exists(),
        "a planted proxy was invoked: {}",
        fs::read_to_string(&marker).unwrap_or_default()
    );

    // Layout and identity.
    let name = format!("0.1.0-{}", &sha[..12]);
    assert_eq!(
        fs::read_link(data.join("current")).unwrap(),
        Path::new(&format!("versions/{name}"))
    );
    assert!(
        data.join("versions")
            .join(&name)
            .join("dotfiles-manager")
            .is_file()
    );
    assert_eq!(
        fs::read_link(home.join(".local/bin/dotfiles-manager")).unwrap(),
        data.join("current/dotfiles-manager")
    );
    for gone in ["cargo-home", "rustup-home", "build-home"] {
        assert!(
            !data.join(gone).exists(),
            "{gone} should be removed after the build"
        );
    }
    assert!(!home.join(".cache/dotfiles-manager/build").exists());

    // Invocation of the installed release binary, which has no test-isolation overrides.
    let bin = home.join(".local/bin/dotfiles-manager");
    let run = |args: &[&str], env: &[(&str, &str)]| {
        let mut c = Command::new(&bin);
        c.env_clear().args(args);
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    };
    let v = String::from_utf8_lossy(&run(&["--version"], &[]).stdout).into_owned();
    assert_eq!(v.trim(), format!("dotfiles-manager 0.1.0 (commit {sha})"));
    assert_eq!(
        run(&["--home", home.to_str().unwrap(), "binding"], &[])
            .status
            .code(),
        Some(2),
        "--home must not exist in a release build"
    );
    let o = run(
        &["binding"],
        &[
            ("HOME", home.to_str().unwrap()),
            ("DFM_HOME", home.to_str().unwrap()),
        ],
    );
    assert_eq!(
        o.status.code(),
        Some(3),
        "DFM_HOME must be ignored in a release build"
    );
    assert!(err(&o).contains("E_HOME_MISMATCH"));
    assert!(
        !home.join(".config").exists(),
        "installation must not bind or apply anything"
    );

    // Clean removal leaves only the owner-placed toolchain.
    let o = script_test_mode(home, &["uninstall"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let left: Vec<_> = entries_below(home)
        .into_iter()
        .filter(|e| !e.starts_with(data.join("toolchains")) && !e.is_dir())
        .collect();
    assert!(left.is_empty(), "files left after uninstall: {left:?}");
    assert!(
        tc.join("bin/cargo").exists(),
        "uninstall must not remove the toolchain"
    );
}

#[test]
#[ignore = "needs crates.io"]
fn upgrade_retries_after_a_stale_current_new_and_rolls_back() {
    let p = prepare("upgrade");
    let (data, first) = (&p.data, p.sha.clone());
    assert_eq!(install(&p, &first, &[]).status.code(), Some(0));
    let second = next_commit(&p, "second");
    // A leftover from an interrupted earlier run: a directory named current.new.
    fs::create_dir_all(data.join("current.new/inner")).unwrap();
    let o = install(&p, &second, &[]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}\n{}",
        String::from_utf8_lossy(&o.stdout),
        err(&o)
    );
    let (n1, n2) = (
        format!("0.1.0-{}", &first[..12]),
        format!("0.1.0-{}", &second[..12]),
    );
    assert_eq!(
        fs::read_link(data.join("current")).unwrap(),
        Path::new(&format!("versions/{n2}"))
    );
    assert!(
        data.join("versions")
            .join(&n1)
            .join("dotfiles-manager")
            .is_file(),
        "the previous version must be kept"
    );
    assert!(!data.join("current.new").exists());
    assert!(
        !data.join("versions").join(&n1).join("current.new").exists(),
        "a link was created inside an old directory"
    );
    // Rollback exactly as documented.
    let rollback = format!(
        "cd '{}' && ln -s versions/{n1} current.new && mv -fh current.new current",
        data.display()
    );
    assert!(
        Command::new("/bin/sh")
            .args(["-c", &rollback])
            .status()
            .unwrap()
            .success()
    );
    let v = Command::new(p.home.join(".local/bin/dotfiles-manager"))
        .env_clear()
        .arg("--version")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&v.stdout).contains(&first));
}

#[test]
#[ignore = "needs crates.io"]
fn a_new_binary_that_cannot_read_the_existing_binding_does_not_become_current() {
    let p = prepare("binding-gate");
    let cfg = p.home.join(".config/dotfiles-manager");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("machine.toml"), "schema = 2\n").unwrap();
    let o = install(&p, &p.sha, &[]);
    assert_eq!(
        o.status.code(),
        Some(1),
        "{}\n{}",
        String::from_utf8_lossy(&o.stdout),
        err(&o)
    );
    assert!(
        !p.data.join("current").exists() && !p.home.join(".local/bin/dotfiles-manager").exists()
    );
    assert_eq!(
        fs::read_to_string(cfg.join("machine.toml")).unwrap(),
        "schema = 2\n",
        "the binding must be untouched"
    );
}

#[test]
#[ignore = "needs crates.io"]
fn the_command_path_and_current_are_checked_again_just_before_the_swap() {
    let p = prepare("swap-recheck");
    // Runs after the build and before the swap; the real race is another process doing this.
    let hook = p.tmp.path().join("hook.sh");
    let link = p.home.join(".local/bin/dotfiles-manager");
    fs::write(
        &hook,
        format!(
            "mkdir -p '{0}' && echo mine > '{1}'\n",
            link.parent().unwrap().display(),
            link.display()
        ),
    )
    .unwrap();
    let o = install(
        &p,
        &p.sha,
        &[("DFM_TEST_BEFORE_SWAP", hook.to_str().unwrap())],
    );
    assert_eq!(
        o.status.code(),
        Some(3),
        "{}\n{}",
        String::from_utf8_lossy(&o.stdout),
        err(&o)
    );
    assert!(err(&o).contains("E_PREREQ_MISSING"), "{}", err(&o));
    assert_eq!(fs::read_to_string(&link).unwrap(), "mine\n");
    assert!(
        !p.data.join("current").exists(),
        "current must not flip when the command path became foreign"
    );
    // Same for `current` itself becoming a directory.
    fs::remove_file(&link).unwrap();
    let hook2 = p.tmp.path().join("hook2.sh");
    fs::write(
        &hook2,
        format!("mkdir -p '{0}'\n", p.data.join("current").display()),
    )
    .unwrap();
    let o = install(
        &p,
        &p.sha,
        &[("DFM_TEST_BEFORE_SWAP", hook2.to_str().unwrap())],
    );
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(err(&o).contains("current"), "{}", err(&o));
}

#[test]
fn missing_command_line_tools_are_refused_before_mutation() {
    let tmp = TempDir::new("clt");
    let home = tmp.path().join("home");
    let toolchain = home.join(".local/share/dotfiles-manager/toolchains/9.9.9");
    fs::create_dir_all(&toolchain).unwrap();
    let (src, sha) = tiny_source(tmp.path(), "9.9.9");
    let before = entries_below(&home);
    let o = script_test_mode(
        &home,
        &[
            "install",
            "--source",
            src.to_str().unwrap(),
            "--commit",
            &sha,
        ],
        &[("DFM_TEST_CLT", "missing")],
    );
    assert_eq!(o.status.code(), Some(3));
    assert!(
        err(&o).contains("E_PREREQ_MISSING") && err(&o).contains("xcode-select --install"),
        "{}",
        err(&o)
    );
    assert_eq!(
        entries_below(&home),
        before,
        "nothing may be created, and no installer may start"
    );
}

#[test]
fn missing_tools_are_detected_before_any_git_call() {
    // /usr/bin/git is the xcode-select shim, so the probe must come first. A directory that is
    // not a git repository would otherwise be refused as an untrusted source.
    let tmp = TempDir::new("clt-order");
    let home = tmp.path().join("home");
    fs::create_dir(&home).unwrap();
    let not_git = tmp.path().join("plain");
    fs::create_dir(&not_git).unwrap();
    let o = script_test_mode(
        &home,
        &[
            "install",
            "--source",
            not_git.to_str().unwrap(),
            "--commit",
            &"a".repeat(40),
        ],
        &[("DFM_TEST_CLT", "missing")],
    );
    assert_eq!(o.status.code(), Some(3));
    assert!(err(&o).contains("E_PREREQ_MISSING"), "{}", err(&o));
    assert!(entries_below(&home).is_empty());
}

/// A home with the toolchain placed, a tiny committed source, and a binding-like path to abuse.
fn refusal_setup(label: &str) -> (TempDir, PathBuf, PathBuf, String) {
    let tmp = TempDir::new(label);
    let home = tmp.path().join("home");
    fs::create_dir_all(home.join(".local/share/dotfiles-manager/toolchains/9.9.9")).unwrap();
    let (src, sha) = tiny_source(tmp.path(), "9.9.9");
    (tmp, home, src, sha)
}

fn install_tiny(home: &Path, src: &Path, sha: &str) -> Output {
    script_test_mode(
        home,
        &[
            "install",
            "--source",
            src.to_str().unwrap(),
            "--commit",
            sha,
        ],
        &[],
    )
}

#[test]
fn an_unusable_binding_stops_the_install_before_any_mutation() {
    use std::os::unix::fs::symlink;
    let (_tmp, home, src, sha) = refusal_setup("binding-pre");
    let cfg = home.join(".config/dotfiles-manager");
    fs::create_dir_all(&cfg).unwrap();
    let binding = cfg.join("machine.toml");
    let valid = "schema = 1\nprofile = \"/p\"\nmachine_id = \"m5\"\nplatform = \"macos\"\narch = \"aarch64\"\norigins = []\n";
    let before = entries_below(&home);
    // Mode 0644: a copy would normalise it to 0600 and hide the problem.
    fs::write(&binding, valid).unwrap();
    fs::set_permissions(&binding, fs::Permissions::from_mode(0o644)).unwrap();
    let o = install_tiny(&home, &src, &sha);
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(
        err(&o).contains("E_BINDING_INVALID") && err(&o).contains("0600"),
        "{}",
        err(&o)
    );
    // A symlink to a valid, private file.
    fs::remove_file(&binding).unwrap();
    let real = home.join("real.toml");
    fs::write(&real, valid).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&real, &binding).unwrap();
    let o = install_tiny(&home, &src, &sha);
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(
        err(&o).contains("E_BINDING_INVALID") && err(&o).contains("symlink"),
        "{}",
        err(&o)
    );
    fs::remove_file(&binding).unwrap();
    fs::remove_file(&real).unwrap();
    assert_eq!(
        entries_below(&home),
        before,
        "nothing may be created, and `current` must stay absent"
    );
}

#[test]
fn a_fifo_at_the_binding_path_does_not_hang_the_script_or_the_manager() {
    let (_tmp, home, src, sha) = refusal_setup("binding-fifo");
    let cfg = home.join(".config/dotfiles-manager");
    fs::create_dir_all(&cfg).unwrap();
    assert!(
        Command::new("/usr/bin/mkfifo")
            .arg(cfg.join("machine.toml"))
            .status()
            .unwrap()
            .success()
    );
    let started = std::time::Instant::now();
    let o = install_tiny(&home, &src, &sha);
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(
        err(&o).contains("E_BINDING_INVALID") && err(&o).contains("regular file"),
        "{}",
        err(&o)
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
}

#[test]
fn an_existing_current_that_is_not_a_symlink_is_refused() {
    let (_tmp, home, src, sha) = refusal_setup("current-dir");
    let current = home.join(".local/share/dotfiles-manager/current");
    fs::create_dir_all(&current).unwrap();
    fs::write(current.join("mine"), "x").unwrap();
    let o = install_tiny(&home, &src, &sha);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("E_PREREQ_MISSING") && err(&o).contains("current"),
        "{}",
        err(&o)
    );
    assert_eq!(fs::read_to_string(current.join("mine")).unwrap(), "x");
    assert!(!home.join(".cache").exists());
}

#[test]
fn a_foreign_file_at_the_command_path_is_never_replaced() {
    let tmp = TempDir::new("foreign");
    let home = tmp.path().join("home");
    fs::create_dir_all(home.join(".local/share/dotfiles-manager/toolchains/9.9.9")).unwrap();
    fs::create_dir_all(home.join(".local/bin")).unwrap();
    let target = home.join(".local/bin/dotfiles-manager");
    fs::write(&target, "mine").unwrap();
    let (src, sha) = tiny_source(tmp.path(), "9.9.9");
    let o = script_test_mode(
        &home,
        &[
            "install",
            "--source",
            src.to_str().unwrap(),
            "--commit",
            &sha,
        ],
        &[],
    );
    assert_eq!(o.status.code(), Some(3));
    assert!(
        err(&o).contains("E_PREREQ_MISSING") && err(&o).contains("not a symlink"),
        "{}",
        err(&o)
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "mine");
    assert!(!home.join(".cache").exists());
}

#[test]
fn uninstall_removes_only_what_the_manager_created() {
    let tmp = TempDir::new("uninstall");
    let home = tmp.path().join("home");
    let cache = home.join(".cache/dotfiles-manager");
    fs::create_dir_all(cache.join("build")).unwrap();
    fs::write(cache.join("build/x"), "x").unwrap();
    fs::write(cache.join("keep.txt"), "mine").unwrap();
    fs::create_dir_all(home.join(".local/bin")).unwrap();
    let foreign = home.join(".local/bin/dotfiles-manager");
    std::os::unix::fs::symlink("/usr/bin/true", &foreign).unwrap();
    let o = script_test_mode(&home, &["uninstall"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(!cache.join("build").exists());
    assert_eq!(fs::read_to_string(cache.join("keep.txt")).unwrap(), "mine");
    assert_eq!(
        fs::read_link(&foreign).unwrap(),
        Path::new("/usr/bin/true"),
        "a link that is not ours must stay"
    );
}
