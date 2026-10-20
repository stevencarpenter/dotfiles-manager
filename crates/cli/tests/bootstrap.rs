//! `scripts/bootstrap.sh`: refusals before mutation, and a full isolated install,
//! invocation and clean removal with a real build.

mod common;

use common::{TempDir, repo_root};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn script(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    script_file(&repo_root().join("scripts/bootstrap.sh"), home, args, env)
}

/// Runs a copy of the script from another path (the self-identity checks look at `$0`).
fn script_file(file: &Path, home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut c = Command::new("/bin/sh");
    c.arg(file)
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
        // maintenance.auto=false: a commit of the repository copy would otherwise detach
        // `git maintenance run --auto` (git 2.54 repacks even with gc.auto=0), which removes the
        // loose objects while a local bare clone is still hardlinking them.
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "maintenance.auto=false",
        ])
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

/// A tiny committed source with only a lock file and the script. Enough for the refusals before any build.
fn tiny_source(tmp: &Path, pin: &str) -> (PathBuf, String) {
    source_with_lock(
        tmp,
        &format!(
            "schema = 1\n\n[manager]\ntoolchain = \"{pin}\"\ntoolchain_tree_sha256 = \"{}\"\n",
            "0".repeat(64)
        ),
    )
}

/// A tiny committed source with a lock the caller writes. The script under test is committed too:
/// install refuses to build a commit whose `scripts/bootstrap.sh` differs from the running file.
fn source_with_lock(tmp: &Path, lock: &str) -> (PathBuf, String) {
    let src = tmp.join("src");
    fs::create_dir_all(src.join("scripts")).unwrap();
    fs::write(src.join("dotfiles-manager.lock"), lock).unwrap();
    fs::copy(
        repo_root().join("scripts/bootstrap.sh"),
        src.join("scripts/bootstrap.sh"),
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
    // The script runs under LC_ALL=C, so the Unicode cases need the byte-level rule that mirrors
    // valid_home in the CLI (char::is_whitespace, char::is_control).
    for name in [
        "a b",
        "a\u{a0}b",   // no-break space
        "a\u{85}b",   // next line (C1 control)
        "a\u{2028}b", // line separator
        "a\u{3000}b", // ideographic space
        "a\u{90}b",   // C1 control that is not White_Space
        "a\u{200a}b", // top of the U+2000..U+200A range
        "a\u{1680}b",
        "a\u{202f}b",
        "a\u{205f}b",
    ] {
        let tmp = TempDir::new("script-space");
        let home = tmp.path().join(name);
        fs::create_dir(&home).unwrap();
        let o = script_test_mode(&home, &["uninstall"], &[]);
        assert_eq!(o.status.code(), Some(3), "{name:?}: {}", err(&o));
        assert!(
            err(&o).contains("E_HOME_MISMATCH") && err(&o).contains("whitespace"),
            "{name:?}: {}",
            err(&o)
        );
        assert!(entries_below(&home).is_empty());
    }
    // Controls: non-ASCII letters, U+00A1, a soft hyphen and a zero-width space (both Cf, not
    // White_Space) are accepted by valid_home, so the script must pass them too.
    for name in ["caf\u{e9}", "a\u{a1}b", "a\u{ad}b", "a\u{200b}b"] {
        let tmp = TempDir::new("script-space");
        let home = tmp.path().join(name);
        fs::create_dir(&home).unwrap();
        let o = script_test_mode(&home, &["uninstall"], &[]);
        assert!(!err(&o).contains("whitespace"), "{name:?}: {}", err(&o));
    }
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
        assert!(
            err(&o).contains("E_TOOLCHAIN_MISMATCH"),
            "{path} at {mode:o}: {}",
            err(&o)
        );
        assert!(o.stdout.is_empty());
        fs::set_permissions(&p, fs::Permissions::from_mode(old)).unwrap();
    }
}

#[test]
fn toolchain_digest_refuses_entries_owned_by_another_user() {
    let tmp = TempDir::new("owner");
    let home = tmp.path().join("home");
    let tc = tmp.path().join("tc");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(tc.join("bin")).unwrap();
    fs::write(tc.join("bin/a"), "a").unwrap();
    std::os::unix::fs::symlink("a", tc.join("bin/l")).unwrap();
    let args = ["toolchain-digest", "--dir", tc.to_str().unwrap()];
    assert_eq!(script_test_mode(&home, &args, &[]).status.code(), Some(0));
    // A test cannot create an entry owned by another user, so the expected owner is another one.
    let other = (dfm_platform_macos::current_uid().unwrap() + 1).to_string();
    let o = script_test_mode(&home, &args, &[("DFM_TEST_UID", &other)]);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(err(&o).contains("E_TOOLCHAIN_MISMATCH"), "{}", err(&o));
    assert!(o.stdout.is_empty());
    // A find that fails (an owner name it cannot resolve) prints nothing; that is not "no foreign entry".
    let o = script_test_mode(&home, &args, &[("DFM_TEST_UID", "no-such-user-dfm")]);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        o.stdout.is_empty(),
        "a digest was printed after a failed owner check"
    );
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
    assert!(err(&o).contains("E_TOOLCHAIN_MISMATCH"), "{}", err(&o));
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
    assert!(
        err(&o).contains("E_SOURCE_UNTRUSTED") && err(&o).contains("filter"),
        "{}",
        err(&o)
    );

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
    assert!(err(&o).contains("E_TOOLCHAIN_MISMATCH"), "{}", err(&o));
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
    assert!(err(&o).contains("E_TOOLCHAIN_MISMATCH"), "{}", err(&o));
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
            // The exact PATH the script requires, so only the allowlist can refuse.
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", &home)
            .env("DFM_BOOTSTRAP_TEST", "1")
            .env("DFM_TEST_HOME", &home);
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    };
    // Control: the marker with a clean environment is accepted and gives the real digest.
    let ok = with_marker(&[]);
    assert_eq!(ok.status.code(), Some(0), "{}", err(&ok));
    assert_eq!(String::from_utf8_lossy(&ok.stdout).trim(), clean);
    let evil_ran = tmp.path().join("evil-ran");
    fs::write(
        forge.join("Evil.pm"),
        format!(
            "package Evil; BEGIN {{ open my $f, '>', '{}'; print $f 'x'; }} 1;\n",
            evil_ran.display()
        ),
    )
    .unwrap();
    for hostile_var in [
        ("PERL5OPT", "-MForge"),
        ("BASH_FUNC_cut%%", "() { echo forged; }"),
        // Functions named like the shell's own test commands must not defeat the guard.
        ("BASH_FUNC_[%%", "() { return 0; }"),
        ("BASH_FUNC_test%%", "() { return 0; }"),
        ("DFM_BS_CLEAN", "1"),
        ("CDPATH", "/tmp"),
    ] {
        let o = with_marker(&[
            ("PERL5LIB", forge.to_str().unwrap()),
            ("PERL5OPT", "-MEvil"),
            hostile_var,
        ]);
        assert_eq!(
            o.status.code(),
            Some(3),
            "{hostile_var:?}: {}",
            String::from_utf8_lossy(&o.stdout)
        );
        assert!(
            o.stdout.is_empty() && err(&o).contains("E_ENV_UNEXPECTED"),
            "{hostile_var:?}: {}",
            err(&o)
        );
        assert!(
            !evil_ran.exists(),
            "{hostile_var:?}: the hostile perl module ran"
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
    prepare_with(label, minimal_toolchain)
}

fn prepare_with(label: &str, make_toolchain: impl FnOnce(&Path) -> String) -> Prepared {
    let tmp = TempDir::new(label);
    let home = tmp.path().join("home");
    let data = home.join(".local/share/dotfiles-manager");
    fs::create_dir_all(&home).unwrap();
    // Owner step: place the exact pinned toolchain and record its tree digest in the lock.
    let tc_tmp = tmp.path().join("tc");
    let pin = make_toolchain(&tc_tmp);
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

/// The live step copies the toolchain with `cp -Rp`. The copy must digest like the original.
#[test]
fn cp_rp_copy_of_a_toolchain_tree_keeps_its_digest() {
    let tmp = TempDir::new("cp-rp");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let orig = tmp.path().join("orig");
    minimal_toolchain(&orig);
    let copy = tmp.path().join("copy");
    let st = Command::new("/bin/cp")
        .arg("-Rp")
        .arg(&orig)
        .arg(&copy)
        .status()
        .unwrap();
    assert!(st.success());
    let want = dfm_cli::toolchain_tree_digest(&orig).unwrap();
    assert_eq!(dfm_cli::toolchain_tree_digest(&copy).unwrap(), want);
    for dir in [&orig, &copy] {
        let o = script_test_mode(
            &home,
            &["toolchain-digest", "--dir", dir.to_str().unwrap()],
            &[],
        );
        assert_eq!(
            String::from_utf8_lossy(&o.stdout).trim(),
            want,
            "{}",
            err(&o)
        );
    }
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
    assert!(data.join("versions").join(&name).join("dfm").is_file());
    assert_eq!(
        fs::read_link(home.join(".local/bin/dfm")).unwrap(),
        data.join("current/dfm")
    );
    for gone in ["cargo-home", "rustup-home", "build-home"] {
        assert!(
            !data.join(gone).exists(),
            "{gone} should be removed after the build"
        );
    }
    assert!(!home.join(".cache/dotfiles-manager/build").exists());

    // Invocation of the installed release binary, which has no test-isolation overrides.
    let bin = home.join(".local/bin/dfm");
    let run = |args: &[&str], env: &[(&str, &str)]| {
        let mut c = Command::new(&bin);
        c.env_clear().args(args);
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    };
    let v = String::from_utf8_lossy(&run(&["--version"], &[]).stdout).into_owned();
    assert_eq!(v.trim(), format!("dfm 0.1.0 (commit {sha})"));
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
    // A leftover from an interrupted earlier run: a dangling symlink named current.new.
    std::os::unix::fs::symlink("versions/gone", data.join("current.new")).unwrap();
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
        data.join("versions").join(&n1).join("dfm").is_file(),
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
    let v = Command::new(p.home.join(".local/bin/dfm"))
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
    // Private and owned by us, so only the new binary's parser can object.
    fs::set_permissions(cfg.join("machine.toml"), fs::Permissions::from_mode(0o600)).unwrap();
    let o = install(&p, &p.sha, &[]);
    assert_eq!(
        o.status.code(),
        Some(1),
        "{}\n{}",
        String::from_utf8_lossy(&o.stdout),
        err(&o)
    );
    assert!(!p.data.join("current").exists() && !p.home.join(".local/bin/dfm").exists());
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
    let link = p.home.join(".local/bin/dfm");
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
        Some(1),
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
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(err(&o).contains("current"), "{}", err(&o));
}

#[test]
#[ignore = "needs crates.io"]
fn the_binding_is_checked_again_just_before_the_swap() {
    let p = prepare("swap-binding");
    let cfg = p.home.join(".config/dotfiles-manager");
    let hook = p.tmp.path().join("hook.sh");
    // After the pre-flip self-check, a binding appears that the new binary would not accept.
    fs::write(
        &hook,
        format!("mkdir -p '{0}' && echo 'schema = 1' > '{0}/machine.toml' && chmod 644 '{0}/machine.toml'\n", cfg.display()),
    )
    .unwrap();
    let o = install(
        &p,
        &p.sha,
        &[("DFM_TEST_BEFORE_SWAP", hook.to_str().unwrap())],
    );
    assert_eq!(
        o.status.code(),
        Some(1),
        "{}\n{}",
        String::from_utf8_lossy(&o.stdout),
        err(&o)
    );
    assert!(err(&o).contains("E_BINDING_INVALID"), "{}", err(&o));
    assert!(!p.data.join("current").exists(), "current must not flip");
}

#[test]
#[ignore = "needs crates.io"]
fn the_binding_is_checked_again_immediately_before_the_copy() {
    let p = prepare("copy-binding");
    let cfg = p.home.join(".config/dotfiles-manager");
    let hook = p.tmp.path().join("hook.sh");
    // A valid binding with mode 0644 appears after the first check. Only the check right before
    // the copy refuses it, with exit 1 because the build homes were reset; without it the new binary's self-check fails with exit 1.
    let valid = "schema = 1\nprofile = \"/p\"\nmachine_id = \"m5\"\nplatform = \"macos\"\narch = \"aarch64\"\norigins = []\n";
    fs::write(
        &hook,
        format!("mkdir -p '{0}' && printf '%s' '{1}' > '{0}/machine.toml' && chmod 644 '{0}/machine.toml'\n", cfg.display(), valid),
    )
    .unwrap();
    let o = install(
        &p,
        &p.sha,
        &[("DFM_TEST_BEFORE_COPY", hook.to_str().unwrap())],
    );
    assert_eq!(
        o.status.code(),
        Some(1),
        "{}\n{}",
        String::from_utf8_lossy(&o.stdout),
        err(&o)
    );
    assert!(
        err(&o).contains("E_BINDING_INVALID") && err(&o).contains("0600"),
        "{}",
        err(&o)
    );
    assert!(!p.data.join("current").exists(), "current must not flip");
}

#[test]
#[ignore = "needs crates.io"]
fn a_swap_that_did_not_take_effect_is_not_reported_as_done() {
    let p = prepare("swap-readback");
    let hook = p.tmp.path().join("hook.sh");
    // After the swap, `current` is replaced by a directory.
    fs::write(
        &hook,
        format!(
            "rm -f '{0}/current' && mkdir '{0}/current'\n",
            p.data.display()
        ),
    )
    .unwrap();
    let o = install(
        &p,
        &p.sha,
        &[("DFM_TEST_AFTER_SWAP", hook.to_str().unwrap())],
    );
    assert_eq!(
        o.status.code(),
        Some(1),
        "{}\n{}",
        String::from_utf8_lossy(&o.stdout),
        err(&o)
    );
    assert!(err(&o).contains("swap did not take effect"), "{}", err(&o));
    assert!(!String::from_utf8_lossy(&o.stdout).contains("bootstrap: done"));
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
fn a_binding_reached_through_an_unusable_config_directory_is_refused_before_any_build() {
    use std::os::unix::fs::symlink;
    let valid = "schema = 1\nprofile = \"/p\"\nmachine_id = \"m5\"\nplatform = \"macos\"\narch = \"aarch64\"\norigins = []\n";
    for case in [
        "symlinked-config",
        "symlinked-leaf",
        "group-writable-leaf",
        "group-writable-config",
    ] {
        let (tmp, home, src, sha) = refusal_setup(&format!("config-tree-{case}"));
        let real = tmp.path().join("real-config");
        fs::create_dir_all(real.join("dotfiles-manager")).unwrap();
        fs::write(real.join("dotfiles-manager/machine.toml"), valid).unwrap();
        fs::set_permissions(
            real.join("dotfiles-manager/machine.toml"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let cfg = home.join(".config");
        let mode =
            |p: &Path, m: u32| fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap();
        match case {
            "symlinked-config" => symlink(&real, &cfg).unwrap(),
            "symlinked-leaf" => {
                fs::create_dir_all(&cfg).unwrap();
                symlink(real.join("dotfiles-manager"), cfg.join("dotfiles-manager")).unwrap();
            }
            "group-writable-leaf" => {
                fs::create_dir_all(cfg.join("dotfiles-manager")).unwrap();
                fs::copy(
                    real.join("dotfiles-manager/machine.toml"),
                    cfg.join("dotfiles-manager/machine.toml"),
                )
                .unwrap();
                mode(&cfg.join("dotfiles-manager/machine.toml"), 0o600);
                mode(&cfg.join("dotfiles-manager"), 0o770);
            }
            _ => {
                fs::create_dir_all(cfg.join("dotfiles-manager")).unwrap();
                fs::copy(
                    real.join("dotfiles-manager/machine.toml"),
                    cfg.join("dotfiles-manager/machine.toml"),
                )
                .unwrap();
                mode(&cfg.join("dotfiles-manager/machine.toml"), 0o600);
                mode(&cfg, 0o775);
            }
        }
        let before = entries_below(&home);
        let o = install_tiny(&home, &src, &sha);
        assert_eq!(o.status.code(), Some(3), "{case}: {}", err(&o));
        let want = match case {
            "symlinked-config" => format!("{} is a symlink", cfg.display()),
            "symlinked-leaf" => format!("{} is a symlink", cfg.join("dotfiles-manager").display()),
            "group-writable-leaf" => {
                format!("{} has mode 770", cfg.join("dotfiles-manager").display())
            }
            _ => format!("{} has mode 775", cfg.display()),
        };
        assert!(
            err(&o).contains("E_PREREQ_MISSING") && err(&o).contains(&want),
            "{case}: want {want}: {}",
            err(&o)
        );
        assert!(
            !home.join(".cache/dotfiles-manager/build").exists(),
            "{case}: a build artefact exists"
        );
        assert_eq!(
            entries_below(&home),
            before,
            "{case}: the refusal changed HOME"
        );
    }
}

#[test]
fn a_home_alias_is_checked_at_its_target() {
    use std::os::unix::fs::symlink;
    for (mode, refused) in [(0o775, true), (0o755, false)] {
        let (tmp, real, src, sha) = refusal_setup(&format!("home-alias-{mode:o}"));
        fs::set_permissions(&real, fs::Permissions::from_mode(mode)).unwrap();
        let alias = tmp.path().join("alias");
        symlink(&real, &alias).unwrap();
        let o = install_tiny(&alias, &src, &sha);
        let want = format!("{} has mode {mode:o}", alias.display());
        if refused {
            assert_eq!(o.status.code(), Some(3), "{}", err(&o));
            assert!(
                err(&o).contains("E_PREREQ_MISSING") && err(&o).contains(&want),
                "{}",
                err(&o)
            );
        } else {
            assert!(
                !err(&o).contains("has mode") && !err(&o).contains("not owned"),
                "{}",
                err(&o)
            );
        }
    }
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
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
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
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
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
fn a_binding_with_a_second_hard_link_stops_the_install_before_any_mutation() {
    let (_tmp, home, src, sha) = refusal_setup("binding-nlink");
    let cfg = home.join(".config/dotfiles-manager");
    fs::create_dir_all(&cfg).unwrap();
    let binding = cfg.join("machine.toml");
    let valid = "schema = 1\nprofile = \"/p\"\nmachine_id = \"m5\"\nplatform = \"macos\"\narch = \"aarch64\"\norigins = []\n";
    let before = entries_below(&home);
    fs::write(&binding, valid).unwrap();
    fs::set_permissions(&binding, fs::Permissions::from_mode(0o600)).unwrap();
    let other = home.join("other-link.toml");
    fs::hard_link(&binding, &other).unwrap();
    let o = install_tiny(&home, &src, &sha);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("E_BINDING_INVALID")
            && err(&o).contains("machine.toml")
            && err(&o).contains("more than one hard link"),
        "{}",
        err(&o)
    );
    fs::remove_file(&binding).unwrap();
    fs::remove_file(&other).unwrap();
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
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
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
fn an_existing_current_new_that_is_not_a_symlink_is_refused() {
    let (_tmp, home, src, sha) = refusal_setup("current-new-dir");
    let leaf = home.join(".local/share/dotfiles-manager/current.new");
    fs::create_dir_all(&leaf).unwrap();
    fs::write(leaf.join("mine"), "x").unwrap();
    let o = install_tiny(&home, &src, &sha);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(err(&o).contains("current.new"), "{}", err(&o));
    assert_eq!(fs::read_to_string(leaf.join("mine")).unwrap(), "x");
    assert!(!home.join(".cache").exists());
}

#[test]
fn a_foreign_file_at_the_command_path_is_never_replaced() {
    let tmp = TempDir::new("foreign");
    let home = tmp.path().join("home");
    fs::create_dir_all(home.join(".local/share/dotfiles-manager/toolchains/9.9.9")).unwrap();
    fs::create_dir_all(home.join(".local/bin")).unwrap();
    let target = home.join(".local/bin/dfm");
    fs::write(&target, "mine").unwrap();
    // The old command link is retired only after a swap; a refused install leaves it.
    let old = home.join(".local/bin/dotfiles-manager");
    std::os::unix::fs::symlink(
        home.join(".local/share/dotfiles-manager/current/dotfiles-manager"),
        &old,
    )
    .unwrap();
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
    assert!(old.is_symlink(), "a refused install retires nothing");
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
    let foreign = home.join(".local/bin/dfm");
    std::os::unix::fs::symlink("/usr/bin/true", &foreign).unwrap();
    // A staging clone left by an interrupted URL install lives inside the build directory.
    fs::create_dir_all(cache.join("build/source.git")).unwrap();
    fs::write(
        cache.join("build/source.git/HEAD"),
        "ref: refs/heads/main\n",
    )
    .unwrap();
    // The pre-DFM-67 command name: ours when it points into `current`, foreign otherwise.
    let data = home.join(".local/share/dotfiles-manager");
    let old = home.join(".local/bin/dotfiles-manager");
    std::os::unix::fs::symlink(data.join("current/dotfiles-manager"), &old).unwrap();
    let o = script_test_mode(&home, &["uninstall"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(!cache.join("build").exists());
    assert_eq!(fs::read_to_string(cache.join("keep.txt")).unwrap(), "mine");
    assert_eq!(
        fs::read_link(&foreign).unwrap(),
        Path::new("/usr/bin/true"),
        "a link that is not ours must stay"
    );
    assert!(
        !old.exists() && !old.is_symlink(),
        "the old command link is ours and goes"
    );
    assert!(String::from_utf8_lossy(&o.stdout).contains("removed the old command link"));

    std::os::unix::fs::symlink("/usr/bin/false", &old).unwrap();
    let o = script_test_mode(&home, &["uninstall"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(
        fs::read_link(&old).unwrap(),
        Path::new("/usr/bin/false"),
        "an old-name link that is not ours must stay"
    );
    assert!(String::from_utf8_lossy(&o.stdout).contains("left alone"));

    // An owner-made alias to the new binary and a regular file are left alone too.
    fs::remove_file(&old).unwrap();
    std::os::unix::fs::symlink(data.join("current/dfm"), &old).unwrap();
    let o = script_test_mode(&home, &["uninstall"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(fs::read_link(&old).unwrap(), data.join("current/dfm"));
    fs::remove_file(&old).unwrap();
    fs::write(&old, "mine").unwrap();
    let o = script_test_mode(&home, &["uninstall"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(fs::read_to_string(&old).unwrap(), "mine");
    assert!(String::from_utf8_lossy(&o.stdout).contains("left alone"));
}

/// Sets up a data tree whose `leaf` is a directory holding a victim file, then uninstalls.
fn uninstall_with_directory_at(leaf: &str) {
    let tmp = TempDir::new("uninstall-leaf");
    let home = tmp.path().join("home");
    let data = home.join(".local/share/dotfiles-manager");
    fs::create_dir_all(data.join("versions/v1")).unwrap();
    fs::create_dir_all(data.join(leaf)).unwrap();
    fs::write(data.join(leaf).join("victim"), "keep").unwrap();
    let o = script_test_mode(&home, &["uninstall"], &[]);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("E_PREREQ_MISSING") && err(&o).contains(leaf),
        "{}",
        err(&o)
    );
    assert_eq!(
        fs::read_to_string(data.join(leaf).join("victim")).unwrap(),
        "keep"
    );
    assert!(
        data.join("versions/v1").is_dir(),
        "nothing may be removed before the refusal"
    );
}

#[test]
fn uninstall_refuses_a_directory_at_current() {
    uninstall_with_directory_at("current");
}

#[test]
fn uninstall_refuses_a_directory_at_current_new() {
    uninstall_with_directory_at("current.new");
}

#[test]
fn uninstall_removes_symlinked_current_and_current_new_without_following() {
    let tmp = TempDir::new("uninstall-links");
    let home = tmp.path().join("home");
    let data = home.join(".local/share/dotfiles-manager");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(data.join("versions/v1")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("victim"), "keep").unwrap();
    std::os::unix::fs::symlink(&outside, data.join("current")).unwrap();
    std::os::unix::fs::symlink("versions/gone", data.join("current.new")).unwrap();
    let o = script_test_mode(&home, &["uninstall"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(fs::symlink_metadata(data.join("current")).is_err());
    assert!(fs::symlink_metadata(data.join("current.new")).is_err());
    assert_eq!(fs::read_to_string(outside.join("victim")).unwrap(), "keep");
}

#[test]
fn the_early_pin_is_read_from_the_manager_table_with_the_staged_grammar() {
    let tree = format!("toolchain_tree_sha256 = \"{}\"\n", "0".repeat(64));
    let tmp = TempDir::new("pin-hint");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    // Indented keys are valid in the restricted grammar, and an earlier table's key is not the pin.
    let lock = format!(
        "schema = 1\n[other]\ntoolchain = \"1.1.1\"\n[manager]\n  toolchain = \"9.9.9\"\n  {tree}"
    );
    let (src, sha) = source_with_lock(tmp.path(), &lock);
    let o = install_tiny(&home, &src, &sha);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("E_TOOLCHAIN_MISMATCH")
            && err(&o).contains("9.9.9")
            && !err(&o).contains("1.1.1"),
        "{}",
        err(&o)
    );
    // No pin in [manager]: refused, even though another table has one.
    let tmp = TempDir::new("pin-hint-none");
    let lock = format!("schema = 1\n[other]\ntoolchain = \"9.9.9\"\n[manager]\n{tree}");
    let (src, sha) = source_with_lock(tmp.path(), &lock);
    let o = install_tiny(&home, &src, &sha);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(err(&o).contains("E_SOURCE_UNTRUSTED"), "{}", err(&o));
    // An array of tables is not the [manager] table; the staged check refuses it as well
    // (fixture srclock-manager-array).
    let tmp = TempDir::new("pin-hint-array");
    let lock = format!("schema = 1\n[[manager]]\ntoolchain = \"9.9.9\"\n{tree}");
    let (src, sha) = source_with_lock(tmp.path(), &lock);
    let o = install_tiny(&home, &src, &sha);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("E_SOURCE_UNTRUSTED") && err(&o).contains("[[manager]]"),
        "{}",
        err(&o)
    );
    // A repeated key is refused, as the staged check refuses it.
    let tmp = TempDir::new("pin-hint-twice");
    let lock =
        format!("schema = 1\n[manager]\ntoolchain = \"9.9.9\"\ntoolchain = \"9.9.9\"\n{tree}");
    let (src, sha) = source_with_lock(tmp.path(), &lock);
    let o = install_tiny(&home, &src, &sha);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(err(&o).contains("E_SOURCE_UNTRUSTED"), "{}", err(&o));
}

#[test]
fn a_symlink_target_with_a_trailing_newline_is_refused_like_the_rust_checker() {
    let tmp = TempDir::new("link-newline");
    let home = tmp.path().join("home");
    let tc = tmp.path().join("tc");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(tc.join("bin")).unwrap();
    fs::write(tc.join("bin/a"), "a").unwrap();
    let link = tc.join("bin/l");
    std::os::unix::fs::symlink("a", &link).unwrap();
    let args = ["toolchain-digest", "--dir", tc.to_str().unwrap()];
    let ok = script_test_mode(&home, &args, &[]);
    assert_eq!(ok.status.code(), Some(0), "{}", err(&ok));
    assert_eq!(
        String::from_utf8_lossy(&ok.stdout).trim(),
        dfm_cli::toolchain_tree_digest(&tc).unwrap()
    );
    for target in ["a\n", "a\n\n", "\n"] {
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(target, &link).unwrap();
        assert!(
            dfm_cli::toolchain_tree_digest(&tc).is_err(),
            "Rust accepted {target:?}"
        );
        let o = script_test_mode(&home, &args, &[]);
        assert_eq!(o.status.code(), Some(3), "shell accepted {target:?}");
        assert!(
            err(&o).contains("E_TOOLCHAIN_MISMATCH"),
            "{target:?}: {}",
            err(&o)
        );
        assert!(o.stdout.is_empty());
    }
}

/// Real data the script must never reach through a link: `outside/<name>/keep`.
fn victim(tmp: &Path, names: &[&str]) -> PathBuf {
    let outside = tmp.join("outside");
    for n in names {
        fs::create_dir_all(outside.join(n)).unwrap();
        fs::write(outside.join(n).join("keep"), "mine").unwrap();
    }
    outside
}

fn assert_victim_intact(outside: &Path, names: &[&str]) {
    for n in names {
        assert_eq!(
            fs::read_to_string(outside.join(n).join("keep")).unwrap(),
            "mine",
            "{n} was changed through a link"
        );
    }
}

#[test]
fn a_symlinked_cache_hierarchy_is_refused_before_any_recursive_change() {
    use std::os::unix::fs::symlink;
    let names = ["build", "dotfiles-manager"];
    // `.cache/dotfiles-manager` is a link, then `.cache` itself, then `build` itself.
    for (case, link, to) in [
        ("leaf-parent", ".cache/dotfiles-manager", "dotfiles-manager"),
        ("cache", ".cache", "."),
        ("build", ".cache/dotfiles-manager/build", "build"),
    ] {
        let (tmp, home, src, sha) = refusal_setup(&format!("cache-link-{case}"));
        let outside = victim(tmp.path(), &names);
        fs::create_dir_all(home.join(link).parent().unwrap()).unwrap();
        symlink(outside.join(to), home.join(link)).unwrap();
        let o = install_tiny(&home, &src, &sha);
        assert_eq!(o.status.code(), Some(3), "install {case}: {}", err(&o));
        assert!(
            err(&o).contains("E_PREREQ_MISSING") && err(&o).contains("symlink"),
            "{case}: {}",
            err(&o)
        );
        let o = script_test_mode(&home, &["uninstall"], &[]);
        assert_eq!(o.status.code(), Some(3), "uninstall {case}: {}", err(&o));
        assert!(err(&o).contains("E_PREREQ_MISSING"), "{case}: {}", err(&o));
        assert_victim_intact(&outside, &names);
    }
}

#[test]
fn a_symlinked_data_hierarchy_is_refused_before_any_recursive_removal() {
    use std::os::unix::fs::symlink;
    let names = [
        "toolchains",
        "versions",
        "cargo-home",
        "rustup-home",
        "build-home",
    ];
    for (case, link) in [
        ("toolchains", ".local/share/dotfiles-manager/toolchains"),
        ("versions", ".local/share/dotfiles-manager/versions"),
        ("cargo-home", ".local/share/dotfiles-manager/cargo-home"),
        ("rustup-home", ".local/share/dotfiles-manager/rustup-home"),
        ("build-home", ".local/share/dotfiles-manager/build-home"),
    ] {
        let (tmp, home, src, sha) = refusal_setup(&format!("data-link-{case}"));
        let outside = victim(tmp.path(), &names);
        let name = link.rsplit('/').next().unwrap();
        if case == "toolchains" {
            // The setup made a real directory; the link must hold the pinned tree the install needs.
            fs::remove_dir_all(home.join(link)).unwrap();
            fs::create_dir_all(outside.join(name).join("9.9.9")).unwrap();
        }
        symlink(outside.join(name), home.join(link)).unwrap();
        let o = install_tiny(&home, &src, &sha);
        assert_eq!(o.status.code(), Some(3), "install {case}: {}", err(&o));
        assert!(err(&o).contains("E_PREREQ_MISSING"), "{case}: {}", err(&o));
        let o = script_test_mode(&home, &["uninstall"], &[]);
        assert_eq!(o.status.code(), Some(3), "uninstall {case}: {}", err(&o));
        assert!(err(&o).contains("E_PREREQ_MISSING"), "{case}: {}", err(&o));
        assert_victim_intact(&outside, &names);
    }
    // The data directory and an ancestor below HOME are links to a tree that holds real data.
    for (case, link) in [
        ("data", ".local/share/dotfiles-manager"),
        ("share", ".local/share"),
    ] {
        let tmp = TempDir::new(&format!("data-link-{case}"));
        let home = tmp.path().join("home");
        let (src, sha) = tiny_source(tmp.path(), "9.9.9");
        let real = tmp.path().join("real");
        fs::create_dir_all(real.join("share/dotfiles-manager/toolchains/9.9.9")).unwrap();
        for n in names {
            fs::create_dir_all(real.join("share/dotfiles-manager").join(n)).unwrap();
            fs::write(
                real.join("share/dotfiles-manager").join(n).join("keep"),
                "mine",
            )
            .unwrap();
        }
        fs::create_dir_all(home.join(".local")).unwrap();
        let to = if case == "data" {
            fs::create_dir_all(home.join(".local/share")).unwrap();
            real.join("share/dotfiles-manager")
        } else {
            real.join("share")
        };
        symlink(to, home.join(link)).unwrap();
        for args in [
            vec!["uninstall"],
            vec![
                "install",
                "--source",
                src.to_str().unwrap(),
                "--commit",
                &sha,
            ],
        ] {
            let o = script_test_mode(&home, &args, &[]);
            assert_eq!(o.status.code(), Some(3), "{case} {args:?}: {}", err(&o));
            assert!(err(&o).contains("E_PREREQ_MISSING"), "{case}: {}", err(&o));
        }
        for n in names {
            assert_eq!(
                fs::read_to_string(real.join("share/dotfiles-manager").join(n).join("keep"))
                    .unwrap(),
                "mine",
                "{case}: {n}"
            );
        }
    }
}

#[test]
#[ignore = "needs crates.io"]
fn install_retires_the_old_command_link_and_keeps_a_foreign_one() {
    let p = prepare("old-link");
    let bin = p.home.join(".local/bin");
    fs::create_dir_all(&bin).unwrap();
    let old = bin.join("dotfiles-manager");
    std::os::unix::fs::symlink(p.data.join("current/dotfiles-manager"), &old).unwrap();
    let o = install(&p, &p.sha, &[]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(
        !old.exists() && !old.is_symlink(),
        "the old command link is retired"
    );
    assert!(String::from_utf8_lossy(&o.stdout).contains("removed the old command link"));
    assert_eq!(
        fs::read_link(bin.join("dfm")).unwrap(),
        p.data.join("current/dfm")
    );

    std::os::unix::fs::symlink("/usr/bin/true", &old).unwrap();
    let o = install(&p, &p.sha, &[]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(
        fs::read_link(&old).unwrap(),
        Path::new("/usr/bin/true"),
        "an old-name link that is not ours must stay"
    );
    assert!(String::from_utf8_lossy(&o.stdout).contains("left alone"));
}

#[test]
#[ignore = "needs crates.io"]
fn a_foreign_dotfiles_manager_new_in_the_bin_directory_is_never_removed() {
    let p = prepare("foreign-new");
    let bin = p.home.join(".local/bin");
    fs::create_dir_all(bin.join("dfm.new/inner")).unwrap();
    fs::write(bin.join("dfm.new/inner/keep"), "mine").unwrap();
    let o = install(&p, &p.sha, &[]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}\n{}",
        String::from_utf8_lossy(&o.stdout),
        err(&o)
    );
    assert_eq!(
        fs::read_to_string(bin.join("dfm.new/inner/keep")).unwrap(),
        "mine"
    );
    assert_eq!(
        fs::read_link(bin.join("dfm")).unwrap(),
        p.data.join("current/dfm")
    );
    let left: Vec<_> = fs::read_dir(&bin)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left.len(), 2, "a staging entry was left behind: {left:?}");
}

#[test]
#[ignore = "needs crates.io"]
fn a_symlinked_version_directory_or_staged_binary_is_not_written_through() {
    use std::os::unix::fs::symlink;
    let p = prepare("version-link");
    let name = format!("0.1.0-{}", &p.sha[..12]);
    let versions = p.data.join("versions");
    fs::create_dir_all(&versions).unwrap();
    let outside = victim(p.tmp.path(), &["target"]);
    // The version directory itself is a link to another tree.
    symlink(outside.join("target"), versions.join(&name)).unwrap();
    let o = install(&p, &p.sha, &[]);
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(
        err(&o).contains("E_PREREQ_MISSING") && err(&o).contains("symlink"),
        "{}",
        err(&o)
    );
    assert!(!outside.join("target/dfm").exists());
    assert!(!p.data.join("current").exists(), "current must not flip");
    // A planted staging name inside a real version directory is a link to a file elsewhere.
    fs::remove_file(versions.join(&name)).unwrap();
    fs::create_dir(versions.join(&name)).unwrap();
    let elsewhere = outside.join("target/keep");
    symlink(&elsewhere, versions.join(&name).join("dfm.new")).unwrap();
    let o = install(&p, &p.sha, &[]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(fs::read_to_string(&elsewhere).unwrap(), "mine");
    let installed = versions.join(&name).join("dfm");
    assert!(installed.is_file() && !installed.is_symlink());
}

#[test]
#[ignore = "needs crates.io"]
fn the_command_link_is_staged_before_current_flips() {
    let p = prepare("link-before-flip");
    let bin = p.home.join(".local/bin");
    // First install: the bin directory becomes unwritable right before the swap. The link cannot
    // be staged, so `current` must not flip.
    let lock = p.tmp.path().join("lock.sh");
    fs::write(&lock, format!("chmod 500 '{}'\n", bin.display())).unwrap();
    let o = install(
        &p,
        &p.sha,
        &[("DFM_TEST_BEFORE_SWAP", lock.to_str().unwrap())],
    );
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(err(&o).contains("command link"), "{}", err(&o));
    assert!(!p.data.join("current").exists(), "current flipped first");
    assert!(!String::from_utf8_lossy(&o.stdout).contains("bootstrap: done"));
    // Upgrade: the link already resolves through `current`, so it is not rewritten and an
    // unwritable bin directory cannot fail the install after the flip.
    assert_eq!(install(&p, &p.sha, &[]).status.code(), Some(0));
    let second = next_commit(&p, "second");
    let o = install(
        &p,
        &second,
        &[("DFM_TEST_BEFORE_SWAP", lock.to_str().unwrap())],
    );
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}\n{}",
        String::from_utf8_lossy(&o.stdout),
        err(&o)
    );
    assert!(String::from_utf8_lossy(&o.stdout).contains("bootstrap: done"));
    assert_eq!(
        fs::read_link(p.data.join("current")).unwrap(),
        Path::new(&format!("versions/0.1.0-{}", &second[..12]))
    );
}

/// git archive would apply checkout-local attributes and run filter commands from the checkout's
/// config. Each case is refused before the toolchain is looked at, with HOME untouched.
#[test]
fn checkout_local_attributes_and_filters_are_refused_before_git_archive_runs() {
    type Setup = fn(&Path, &Path);
    let cases: [(&str, Setup); 4] = [
        ("info-attributes", |src, _| {
            fs::write(
                src.join(".git/info/attributes"),
                "dotfiles-manager.lock export-ignore\n",
            )
            .unwrap();
        }),
        ("attributes-file", |src, _| {
            git(
                src,
                &["config", "core.attributesFile", "/tmp/dfm-attributes"],
            );
        }),
        ("smudge-filter", |src, marker| {
            fs::write(src.join(".gitattributes"), "* filter=ev\n").unwrap();
            git(src, &["add", ".gitattributes"]);
            git(src, &["commit", "-q", "-m", "attributes"]);
            git(
                src,
                &[
                    "config",
                    "filter.ev.smudge",
                    &format!("touch {}", marker.display()),
                ],
            );
        }),
        ("clean-filter", |src, marker| {
            git(
                src,
                &[
                    "config",
                    "filter.ev.clean",
                    &format!("touch {}", marker.display()),
                ],
            );
        }),
    ];
    for (name, setup) in cases {
        let tmp = TempDir::new(name);
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let (src, _) = tiny_source(tmp.path(), "9.9.9");
        let marker = tmp.path().join("filter-ran");
        setup(&src, &marker);
        let sha = git(&src, &["rev-parse", "HEAD"]);
        let o = install_tiny(&home, &src, &sha);
        assert_eq!(o.status.code(), Some(3), "{name}: {}", err(&o));
        assert!(
            err(&o).contains("E_SOURCE_UNTRUSTED") && !err(&o).contains("E_TOOLCHAIN_MISMATCH"),
            "{name}: {}",
            err(&o)
        );
        assert!(!marker.exists(), "{name}: a filter ran");
        assert!(
            entries_below(&home).is_empty(),
            "{name}: refusal must not write"
        );
    }
}

/// git archive applies core.autocrlf from the checkout's config, so the script pins it on the
/// command line and the staged tree keeps the committed bytes: a stand-in `cargo fetch` reports a
/// carriage return in the staged manifest. The script also pins core.eol=lf, but with git 2.54 a
/// checkout's core.eol alone does not change `git archive` output (checked with a committed
/// `* text` attribute), so no test can show that pin; it stays as defence for other git versions.
/// Hostile config that no git call of the script uses (fsmonitor, hooks) is set too: it must not run.
#[test]
fn checkout_local_line_ending_config_cannot_change_the_staged_tree() {
    type Setup = fn(&Path);
    let cases: [(&str, Setup); 1] = [("autocrlf", |src| {
        git(src, &["config", "core.autocrlf", "true"]);
    })];
    let on_fetch =
        r#"if [ -n "$(tr -cd '\r' <stage/Cargo.toml)" ]; then echo STAGED-CR >&2; fi; exit 1"#;
    for (name, setup) in cases {
        let p = prepare_with(name, fake_toolchain(on_fetch.into()));
        setup(&p.src);
        // Set after setup: the test's own git calls must not run it.
        let marker = p.tmp.path().join("config-ran");
        let hooks = p.tmp.path().join("hooks");
        fs::create_dir(&hooks).unwrap();
        let fsmonitor = format!("touch {}", marker.display());
        git(&p.src, &["config", "core.fsmonitor", &fsmonitor]);
        git(
            &p.src,
            &["config", "core.hooksPath", hooks.to_str().unwrap()],
        );
        let sha = git(&p.src, &["rev-parse", "HEAD"]);
        let o = install(&p, &sha, &[]);
        assert!(
            !err(&o).contains("STAGED-CR"),
            "{name}: the staged tree has line endings the commit does not: {}",
            err(&o)
        );
        assert_eq!(o.status.code(), Some(1), "{name}: {}", err(&o));
        assert!(
            err(&o).contains("cargo fetch failed"),
            "{name}: {}",
            err(&o)
        );
        assert!(!marker.exists(), "{name}: checkout config ran code");
        assert!(build_dirs_gone(&p), "{name}");
        assert!(!p.data.join("current").exists(), "{name}");
    }
}

/// An unreadable git configuration must refuse, not read as "no attributes and no filters". The
/// configuration is read first, so an include.path that names a directory reaches this refusal.
#[test]
fn an_unreadable_checkout_config_is_refused_before_anything_else() {
    let (tmp, home, src, sha) = refusal_setup("bad-config");
    let dir = tmp.path().join("include-dir");
    fs::create_dir(&dir).unwrap();
    git(&src, &["config", "include.path", dir.to_str().unwrap()]);
    let before = snapshot(&home);
    let o = install_tiny(&home, &src, &sha);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("E_SOURCE_UNTRUSTED")
            && err(&o).contains("cannot read the git configuration")
            && err(&o).contains("git exit 128"),
        "{}",
        err(&o)
    );
    assert_eq!(snapshot(&home), before);
}

/// A source directory that is not a git checkout passes the configuration read (git finds no
/// repository configuration and exits 1), so the git directory read is what refuses it.
#[test]
fn a_source_that_is_not_a_git_checkout_is_refused_for_its_git_directory() {
    let (tmp, home, _src, sha) = refusal_setup("not-a-checkout");
    let plain = tmp.path().join("plain");
    fs::create_dir(&plain).unwrap();
    let before = snapshot(&home);
    let o = install_tiny(&home, &plain, &sha);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("E_SOURCE_UNTRUSTED") && err(&o).contains("cannot read the git directory"),
        "{}",
        err(&o)
    );
    assert_eq!(snapshot(&home), before);
}

#[test]
fn a_symlinked_bin_directory_is_refused_before_the_command_link_is_staged() {
    let (tmp, home, src, sha) = refusal_setup("bin-link");
    let outside = victim(tmp.path(), &["bin"]);
    std::os::unix::fs::symlink(outside.join("bin"), home.join(".local/bin")).unwrap();
    let o = install_tiny(&home, &src, &sha);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("E_PREREQ_MISSING") && err(&o).contains("symlink"),
        "{}",
        err(&o)
    );
    assert_victim_intact(&outside, &["bin"]);
    assert_eq!(fs::read_dir(outside.join("bin")).unwrap().count(), 1);
    assert!(!home.join(".cache").exists());
}

#[test]
#[ignore = "needs crates.io"]
fn the_data_directories_are_checked_again_before_the_late_removals() {
    let p = prepare("late-recheck");
    let outside = victim(p.tmp.path(), &["cargo-home"]);
    // After the swap, the directory about to be removed becomes a link to real data.
    let hook = p.tmp.path().join("hook.sh");
    fs::write(
        &hook,
        format!(
            "rm -rf '{0}/cargo-home' && ln -s '{1}' '{0}/cargo-home'\n",
            p.data.display(),
            outside.join("cargo-home").display()
        ),
    )
    .unwrap();
    let o = install(
        &p,
        &p.sha,
        &[("DFM_TEST_AFTER_SWAP", hook.to_str().unwrap())],
    );
    // The swap already happened, so the failed re-check is exit 1, not a refusal with exit 3.
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(
        err(&o).contains("the swap took effect") && err(&o).contains("E_PREREQ_MISSING"),
        "{}",
        err(&o)
    );
    assert!(!String::from_utf8_lossy(&o.stdout).contains("bootstrap: done"));
    assert!(
        fs::read_link(p.data.join("current"))
            .unwrap()
            .starts_with("versions/"),
        "the new version stays current"
    );
    assert!(
        !p.home.join(".cache/dotfiles-manager/build").exists(),
        "the build directory is removed before the exit"
    );
    assert_victim_intact(&outside, &["cargo-home"]);
}

/// Every path under `dir` with its mode, so a refusal can prove it changed nothing.
fn snapshot(dir: &Path) -> Vec<(PathBuf, u32)> {
    let mut v: Vec<_> = entries_below(dir)
        .into_iter()
        .map(|p| {
            let m = fs::symlink_metadata(&p).unwrap().permissions().mode();
            (p, m)
        })
        .collect();
    v.sort();
    v
}

#[test]
fn a_manager_directory_open_to_group_or_others_stops_install_and_uninstall() {
    let rels = [
        ".local",
        ".local/share",
        ".local/bin",
        ".cache",
        ".local/share/dotfiles-manager",
        ".local/share/dotfiles-manager/toolchains",
        ".local/share/dotfiles-manager/versions",
        ".local/share/dotfiles-manager/cargo-home",
        ".local/share/dotfiles-manager/rustup-home",
        ".local/share/dotfiles-manager/build-home",
        ".cache/dotfiles-manager",
        ".cache/dotfiles-manager/build",
    ];
    for rel in rels {
        for mode in [0o775, 0o757] {
            let (tmp, home, src, sha) = refusal_setup("open-dir");
            let dir = home.join(rel);
            fs::create_dir_all(&dir).unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).unwrap();
            let before = snapshot(&home);
            let install = install_tiny(&home, &src, &sha);
            let uninstall = script_test_mode(&home, &["uninstall"], &[]);
            for (what, o) in [("install", install), ("uninstall", uninstall)] {
                assert_eq!(
                    o.status.code(),
                    Some(3),
                    "{what} {rel} {mode:o}: {}",
                    err(&o)
                );
                assert!(
                    err(&o).contains("E_PREREQ_MISSING")
                        && err(&o).contains(dir.to_str().unwrap())
                        && err(&o).contains("writable by group or others"),
                    "{what} {rel} {mode:o}: {}",
                    err(&o)
                );
            }
            assert_eq!(
                snapshot(&home),
                before,
                "{rel} {mode:o}: refusal must not write"
            );
            drop(tmp);
        }
    }
}

/// The owner branch cannot be reached with a real foreign directory, so the expected owner is
/// another user id (test mode only). The first existing component below HOME is the one named.
/// Uninstall refuses HOME itself first (check_home_dir uses the same override), so its own test
/// follows; install checks the binding chain, which always uses the real user id, before the rest.
#[test]
fn a_directory_below_home_owned_by_another_user_stops_install() {
    let (tmp, home, src, sha) = refusal_setup("foreign-owner");
    let before = snapshot(&home);
    let other = (dfm_platform_macos::current_uid().unwrap() + 1).to_string();
    let o = script_test_mode(
        &home,
        &[
            "install",
            "--source",
            src.to_str().unwrap(),
            "--commit",
            &sha,
        ],
        &[("DFM_TEST_UID", other.as_str())],
    );
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("E_PREREQ_MISSING")
            && err(&o).contains(home.join(".local").to_str().unwrap())
            && err(&o).contains("is not owned by the current user"),
        "{}",
        err(&o)
    );
    assert_eq!(snapshot(&home), before, "refusal must not write");
    drop(tmp);
}

/// HOME owned by another user is refused by uninstall before any tree below it is checked or removed.
#[test]
fn uninstall_refuses_a_home_owned_by_another_user_and_removes_nothing() {
    let (tmp, home, _src, _sha) = refusal_setup("foreign-home");
    fs::create_dir_all(home.join(".cache/dotfiles-manager/build")).unwrap();
    let before = snapshot(&home);
    let other = (dfm_platform_macos::current_uid().unwrap() + 1).to_string();
    let o = script_test_mode(&home, &["uninstall"], &[("DFM_TEST_UID", other.as_str())]);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("E_PREREQ_MISSING")
            && err(&o).contains(&format!(
                "{} is not owned by the current user",
                home.display()
            )),
        "{}",
        err(&o)
    );
    assert_eq!(snapshot(&home), before, "refusal must not write");
    drop(tmp);
}

#[test]
fn a_symlinked_bin_directory_is_refused_by_uninstall_and_nothing_is_removed() {
    let (tmp, home, _src, _sha) = refusal_setup("bin-link-uninstall");
    let outside = victim(tmp.path(), &["bin"]);
    let ours = home.join(".local/share/dotfiles-manager/current/dfm");
    let planted = outside.join("bin/dfm");
    std::os::unix::fs::symlink(&ours, &planted).unwrap();
    std::os::unix::fs::symlink(outside.join("bin"), home.join(".local/bin")).unwrap();
    let before = snapshot(&home);
    let o = script_test_mode(&home, &["uninstall"], &[]);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("E_PREREQ_MISSING") && err(&o).contains("symlink"),
        "{}",
        err(&o)
    );
    assert_eq!(fs::read_link(&planted).unwrap(), ours);
    assert_victim_intact(&outside, &["bin"]);
    assert_eq!(snapshot(&home), before);
}

/// Another user who can write HOME could rename `.local` and plant a link before the removals.
#[test]
fn uninstall_refuses_a_home_open_to_group_or_others_and_removes_nothing() {
    for mode in [0o775, 0o757] {
        let tmp = TempDir::new("uninstall-home-open");
        let home = tmp.path().join("home");
        let kept = home.join(".local/share/dotfiles-manager/versions/v1/dotfiles-manager");
        fs::create_dir_all(kept.parent().unwrap()).unwrap();
        fs::write(&kept, "x").unwrap();
        fs::create_dir_all(home.join(".cache/dotfiles-manager/build")).unwrap();
        let before = snapshot(&home);
        fs::set_permissions(&home, fs::Permissions::from_mode(mode)).unwrap();
        let o = script_test_mode(&home, &["uninstall"], &[]);
        fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(o.status.code(), Some(3), "{mode:o}: {}", err(&o));
        assert!(
            err(&o).contains("E_PREREQ_MISSING")
                && err(&o).contains(home.to_str().unwrap())
                && err(&o).contains("writable by group or others"),
            "{mode:o}: {}",
            err(&o)
        );
        assert!(kept.exists(), "{mode:o}: uninstall removed a file");
        assert_eq!(snapshot(&home), before, "{mode:o}");
    }
}

#[test]
fn uninstall_leaves_the_bin_directory_and_removes_the_empty_directories_it_created() {
    let tmp = TempDir::new("uninstall-bin");
    let home = tmp.path().join("home");
    let bin = home.join(".local/bin");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(home.join(".local/share/dotfiles-manager")).unwrap();
    fs::create_dir_all(home.join(".cache/dotfiles-manager/build")).unwrap();
    let o = script_test_mode(&home, &["uninstall"], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(
        bin.is_dir(),
        "~/.local/bin is not the installer's to remove"
    );
    assert!(!home.join(".local/share/dotfiles-manager").exists());
    assert!(!home.join(".cache/dotfiles-manager").exists());
}

/// A pin directory that holds the copy one level down (cp -R into an existing destination) is
/// named in the refusal, and the failed run leaves no build directory behind.
#[test]
fn a_nested_toolchain_copy_is_named_and_a_refused_install_removes_its_build_directory() {
    let p = prepare("nested-toolchain");
    let pin = p.tc.file_name().unwrap().to_str().unwrap().to_string();
    let moved = p.tmp.path().join("moved");
    fs::rename(&p.tc, &moved).unwrap();
    fs::create_dir(&p.tc).unwrap();
    let nested = p.tc.join(format!("{pin}-aarch64-apple-darwin"));
    fs::rename(&moved, &nested).unwrap();
    let o = install(&p, &p.sha, &[]);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("E_TOOLCHAIN_MISMATCH") && err(&o).contains(nested.to_str().unwrap()),
        "{}",
        err(&o)
    );
    assert!(
        !p.home.join(".cache/dotfiles-manager/build").exists(),
        "the build directory must not outlive a refused install"
    );
}

/// A toolchain whose `cargo` is a shell script, so the build steps run without network access.
/// `fetch` runs `on_fetch` (shell text); `build` writes a stand-in binary that reports the
/// identity the script requires and passes `self-check`.
fn fake_toolchain(on_fetch: String) -> impl FnOnce(&Path) -> String {
    move |dest| {
        let bin = dest.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let cargo = format!(
            r#"#!/bin/sh
case $1 in
  fetch) {on_fetch} ;;
  build)
    while [ $# -gt 0 ]; do [ "$1" = --target-dir ] && t=$2; shift; done
    mkdir -p "$t/release"
    printf '#!/bin/sh\ncase $1 in --version) echo "dfm 0.1.0 (commit %s)" ;; esac\n' "$DFM_BUILD_COMMIT" >"$t/release/dfm"
    chmod 755 "$t/release/dfm" ;;
esac
"#
        );
        fs::write(bin.join("cargo"), cargo).unwrap();
        fs::write(bin.join("rustc"), "#!/bin/sh\n").unwrap();
        for f in ["cargo", "rustc"] {
            fs::set_permissions(bin.join(f), fs::Permissions::from_mode(0o755)).unwrap();
        }
        "9.9.9".to_string()
    }
}

fn build_dirs_gone(p: &Prepared) -> bool {
    !p.home.join(".cache/dotfiles-manager/build").exists()
        && ["cargo-home", "rustup-home", "build-home"]
            .iter()
            .all(|d| !p.data.join(d).exists())
}

#[test]
fn a_successful_install_with_a_stand_in_cargo_leaves_no_build_directories() {
    let p = prepare_with("fake-ok", fake_toolchain("exit 0".into()));
    let o = install(&p, &p.sha, &[]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(build_dirs_gone(&p));
    assert!(p.data.join("current").is_symlink());
}

#[test]
fn a_failed_fetch_removes_the_build_directory_and_the_build_homes() {
    // The stand-in fails only when the homes exist, so the cleanup has something to remove.
    let on_fetch =
        r#"test -d "$CARGO_HOME" && test -d "$RUSTUP_HOME" && test -d "$HOME" && exit 1; exit 9"#;
    let p = prepare_with("fake-fetch-fails", fake_toolchain(on_fetch.into()));
    let o = install(&p, &p.sha, &[]);
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(err(&o).contains("cargo fetch failed"), "{}", err(&o));
    assert!(build_dirs_gone(&p), "the failed build must clean up");
    assert!(!p.data.join("current").exists());
}

#[test]
fn sigint_and_sigterm_during_the_build_clean_up_and_exit_with_the_signal_code() {
    for (sig, code) in [("INT", 130), ("TERM", 143)] {
        // cargo is a child of the install shell: it signals its parent, which runs the trap
        // when the child has ended.
        let p = prepare_with(
            "fake-signal",
            fake_toolchain(format!("kill -{sig} $PPID; exit 1")),
        );
        let o = install(&p, &p.sha, &[]);
        assert_eq!(o.status.code(), Some(code), "{sig}: {}", err(&o));
        assert!(build_dirs_gone(&p), "{sig}: the build directories stay");
        assert!(!p.data.join("current").exists(), "{sig}");
    }
}

#[test]
fn a_group_writable_version_directory_stops_install_before_the_binary_is_staged() {
    let p = prepare_with("fake-version-dir", fake_toolchain("exit 0".into()));
    let name = format!("0.1.0-{}", &p.sha[..12]);
    let vdir = p.data.join("versions").join(&name);
    fs::create_dir_all(&vdir).unwrap();
    fs::set_permissions(&vdir, fs::Permissions::from_mode(0o775)).unwrap();
    let o = install(&p, &p.sha, &[]);
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(
        err(&o).contains("E_PREREQ_MISSING") && err(&o).contains("writable by group or others"),
        "{}",
        err(&o)
    );
    assert!(!p.data.join("current").exists());
    assert!(!p.home.join(".local/bin/dfm").exists());
    assert!(
        fs::read_dir(&vdir).unwrap().next().is_none(),
        "nothing staged"
    );
    assert!(build_dirs_gone(&p));
}

/// The binding check right before the copy runs after the build homes were reset, so it exits 1.
#[test]
fn a_refusal_after_the_build_homes_were_reset_exits_1() {
    let p = prepare_with("fake-copy-window", fake_toolchain("exit 0".into()));
    let cfg = p.home.join(".config/dotfiles-manager");
    let hook = p.tmp.path().join("hook.sh");
    let valid = "schema = 1\nprofile = \"/p\"\nmachine_id = \"m5\"\nplatform = \"macos\"\narch = \"aarch64\"\norigins = []\n";
    fs::write(
        &hook,
        format!("mkdir -p '{0}' && printf '%s' '{1}' > '{0}/machine.toml' && chmod 644 '{0}/machine.toml'\n", cfg.display(), valid),
    )
    .unwrap();
    let o = install(
        &p,
        &p.sha,
        &[("DFM_TEST_BEFORE_COPY", hook.to_str().unwrap())],
    );
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(err(&o).contains("E_BINDING_INVALID"), "{}", err(&o));
    assert!(!p.data.join("current").exists());
}

/// The refusal comes after `mkdir -p versions/<name>`, so it is a failure (exit 1), not exit 3.
#[test]
fn a_refusal_after_the_first_mutation_exits_1() {
    let p = prepare_with("fake-binary-dir", fake_toolchain("exit 0".into()));
    let name = format!("0.1.0-{}", &p.sha[..12]);
    let vdir = p.data.join("versions").join(&name);
    fs::create_dir_all(vdir.join("dfm")).unwrap();
    let o = install(&p, &p.sha, &[]);
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(
        err(&o).contains("E_PREREQ_MISSING") && err(&o).contains("is a directory"),
        "{}",
        err(&o)
    );
    assert!(!p.data.join("current").exists());
}

/// After the swap a failed check of one build home leaves that home and removes the others,
/// and a mode problem in `versions` (which cleanup does not touch) stops nothing.
#[test]
fn the_build_directories_are_cleaned_one_by_one_after_the_swap() {
    let p = prepare_with("fake-per-tree", fake_toolchain("exit 0".into()));
    let outside = victim(p.tmp.path(), &["cargo-home"]);
    let hook = p.tmp.path().join("hook.sh");
    fs::write(
        &hook,
        format!(
            "rm -rf '{0}/cargo-home' && ln -s '{1}' '{0}/cargo-home'\nchmod 775 '{0}/versions'\n",
            p.data.display(),
            outside.join("cargo-home").display()
        ),
    )
    .unwrap();
    let o = install(
        &p,
        &p.sha,
        &[("DFM_TEST_AFTER_SWAP", hook.to_str().unwrap())],
    );
    assert_eq!(o.status.code(), Some(1), "{}", err(&o));
    assert!(
        err(&o).contains("the swap took effect") && err(&o).contains("cargo-home"),
        "{}",
        err(&o)
    );
    assert!(
        !err(&o).contains("rustup-home") && !err(&o).contains("build-home"),
        "only the bad tree is named: {}",
        err(&o)
    );
    assert!(
        p.data.join("cargo-home").is_symlink(),
        "the link is neither followed nor removed"
    );
    assert!(!p.data.join("rustup-home").exists() && !p.data.join("build-home").exists());
    assert!(!p.home.join(".cache/dotfiles-manager/build").exists());
    assert_victim_intact(&outside, &["cargo-home"]);
}

#[test]
fn repeated_flags_and_flags_the_command_does_not_use_are_usage_errors() {
    let tmp = TempDir::new("flags");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    for (args, want) in [
        (vec!["uninstall", "--source", "x"], "does not use --source"),
        (
            vec!["check-controls", "--commit", "x"],
            "does not use --commit",
        ),
        (
            vec!["toolchain-digest", "--source", "x"],
            "does not use --source",
        ),
        (
            vec!["check-controls", "--source", "a", "--source", "b"],
            "given twice",
        ),
        (
            vec!["install", "--commit", "a", "--commit", "b"],
            "given twice",
        ),
        // An empty --source is not "absent": only a missing flag means the canonical origin.
        (
            vec!["install", "--source", "", "--commit", "a"],
            "--source needs a non-empty value",
        ),
    ] {
        let o = script_test_mode(&home, &args, &[]);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", err(&o));
        assert!(err(&o).contains(want), "{args:?}: {}", err(&o));
    }
    assert!(entries_below(&home).is_empty());
}

fn sha256_hex(file: &Path) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(fs::read(file).unwrap()))
}

/// `--expect-sha256` is the digest the owner holds out of band. It is checked before any command
/// runs, against the file that executes, so an edited copy is refused with nothing written.
#[test]
fn expect_sha256_accepts_the_matching_digest_and_refuses_a_modified_script() {
    let tmp = TempDir::new("self-check");
    let home = tmp.path().join("home");
    fs::create_dir(&home).unwrap();
    let real = repo_root().join("scripts/bootstrap.sh");
    let digest = sha256_hex(&real);
    let o = script_test_mode(&home, &["uninstall", "--expect-sha256", &digest], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    // The same digest against an edited copy, run from its own path.
    let copy = tmp.path().join("copy/bootstrap.sh");
    fs::create_dir_all(copy.parent().unwrap()).unwrap();
    let mut text = fs::read(&real).unwrap();
    text.extend_from_slice(b"\n# edited\n");
    fs::write(&copy, text).unwrap();
    let env = [
        ("DFM_BOOTSTRAP_TEST", "1"),
        ("DFM_TEST_HOME", home.to_str().unwrap()),
    ];
    let o = script_file(
        &copy,
        &home,
        &["uninstall", "--expect-sha256", &digest],
        &env,
    );
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("E_SOURCE_UNTRUSTED")
            && err(&o).contains(copy.to_str().unwrap())
            && err(&o).contains(&format!("expected {digest}")),
        "{}",
        err(&o)
    );
    assert!(!String::from_utf8_lossy(&o.stdout).contains("removed the installed manager"));
    // The edited copy with its own digest runs: the value, not the path, is what is checked.
    let o = script_file(
        &copy,
        &home,
        &["uninstall", "--expect-sha256", &sha256_hex(&copy)],
        &env,
    );
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    // A value that is not 64 lowercase hex digits is a usage error, checked before the digest.
    for bad in [&digest[..63], "G", &digest.to_uppercase()] {
        let o = script_test_mode(&home, &["uninstall", "--expect-sha256", bad], &[]);
        assert_eq!(o.status.code(), Some(2), "{bad:?}: {}", err(&o));
    }
    // install with a wrong value is refused before the platform, tools or source are looked at.
    let o = script_test_mode(
        &home,
        &[
            "install",
            "--source",
            "/nonexistent",
            "--commit",
            &"a".repeat(40),
            "--expect-sha256",
            &"0".repeat(64),
        ],
        &[("DFM_TEST_OS", "Linux")],
    );
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("E_SOURCE_UNTRUSTED") && !err(&o).contains("E_PLATFORM_UNSUPPORTED"),
        "{}",
        err(&o)
    );
    assert!(entries_below(&home).is_empty());
}

/// install builds only a commit whose `scripts/bootstrap.sh` is the file that runs: a stale or
/// edited script cannot build another commit, and a commit without the script is refused.
#[test]
fn install_refuses_a_commit_whose_script_differs_from_the_running_one() {
    let (_tmp, home, src, good) = refusal_setup("committed-script");
    let before = snapshot(&home);
    let script = src.join("scripts/bootstrap.sh");
    let mut text = fs::read(&script).unwrap();
    text.extend_from_slice(b"\n# edited\n");
    fs::write(&script, text).unwrap();
    git(&src, &["commit", "-q", "-a", "-m", "edited"]);
    let edited = git(&src, &["rev-parse", "HEAD"]);
    let o = install_tiny(&home, &src, &edited);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("E_SOURCE_UNTRUSTED")
            && err(&o).contains("is not scripts/bootstrap.sh of commit")
            && err(&o).contains(&edited),
        "{}",
        err(&o)
    );
    git(&src, &["rm", "-q", "scripts/bootstrap.sh"]);
    git(&src, &["commit", "-q", "-m", "removed"]);
    let removed = git(&src, &["rev-parse", "HEAD"]);
    let o = install_tiny(&home, &src, &removed);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("has no scripts/bootstrap.sh"),
        "{}",
        err(&o)
    );
    assert_eq!(snapshot(&home), before, "refusals must not write");
    // Control: the commit that carries this exact script passes the check and reaches the staged
    // controls (the tiny source has no Cargo.lock).
    let o = install_tiny(&home, &src, &good);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("Cargo.lock missing") && !err(&o).contains("scripts/bootstrap.sh"),
        "{}",
        err(&o)
    );
}

fn install_from(p: &Prepared, source: &str, sha: &str) -> Output {
    script_test_mode(
        &p.home,
        &["install", "--source", source, "--commit", sha],
        &[],
    )
}

/// A URL source is cloned (bare) into the build directory, the pinned commit is built from the
/// clone, and the clone goes with the build directory: after success, after a refusal, and on
/// uninstall (`uninstall_removes_only_what_the_manager_created`). No network: a `file://` origin.
#[test]
fn a_url_source_is_cloned_at_the_exact_commit_and_the_clone_is_removed() {
    let p = prepare_with("clone", fake_toolchain("exit 0".into()));
    let origin = p.tmp.path().join("origin.git");
    git(
        p.tmp.path(),
        &[
            "clone",
            "--bare",
            "-q",
            p.src.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    );
    let url = format!("file://{}", origin.display());
    // A commit the origin does not have: refused after the clone, and the clone is removed.
    let elsewhere = TempDir::new("clone-elsewhere");
    let (_, foreign) = tiny_source(elsewhere.path(), "9.9.9");
    let o = install_from(&p, &url, &foreign);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("E_SOURCE_UNTRUSTED") && err(&o).contains("is not a commit"),
        "{}",
        err(&o)
    );
    assert!(
        build_dirs_gone(&p),
        "the staging clone must not outlive a refusal"
    );
    assert!(!p.data.join("current").exists());
    // An origin that cannot be cloned.
    let o = install_from(&p, "file:///nonexistent-dfm-origin.git", &p.sha);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(err(&o).contains("cannot clone"), "{}", err(&o));
    assert!(build_dirs_gone(&p));
    // The pinned commit from the origin.
    let o = install_from(&p, &url, &p.sha);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}\n{}",
        String::from_utf8_lossy(&o.stdout),
        err(&o)
    );
    assert!(String::from_utf8_lossy(&o.stdout).contains(&format!("source commit {}", p.sha)));
    assert_eq!(
        fs::read_link(p.data.join("current")).unwrap(),
        Path::new(&format!("versions/0.1.0-{}", &p.sha[..12]))
    );
    assert!(
        build_dirs_gone(&p),
        "the staging clone must go with the build directory"
    );
    // git percent-decodes file:// URLs, so a local path with a % must be encoded by the script.
    let percent = p.tmp.path().join("src%41");
    fs::rename(&p.src, &percent).unwrap();
    let o = install_from(&p, percent.to_str().unwrap(), &p.sha);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    fs::rename(&percent, &p.src).unwrap();
    // Without --source the canonical origin would be cloned, after the local checks: the missing
    // tools refusal comes first, so no network is touched here. The source is named up front.
    let o = script_test_mode(
        &p.home,
        &["install", "--commit", &p.sha],
        &[("DFM_TEST_CLT", "missing")],
    );
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("E_PREREQ_MISSING")
            && err(&o)
                .contains("the source is https://github.com/stevencarpenter/dotfiles-manager.git"),
        "{}",
        err(&o)
    );
    // Other transports are refused before any clone is attempted.
    for bad in [
        "ssh://git@example.invalid/x.git",
        "git://example.invalid/x.git",
    ] {
        let o = install_from(&p, bad, &p.sha);
        assert_eq!(o.status.code(), Some(3), "{bad}: {}", err(&o));
        assert!(
            err(&o).contains("E_SOURCE_UNTRUSTED") && err(&o).contains("https:// URL"),
            "{bad}: {}",
            err(&o)
        );
        assert!(build_dirs_gone(&p), "{bad}: a clone was attempted");
    }
}

/// git does not verify a loose object's hash on read: a replaced object file under `.git/objects`
/// of a local checkout makes `rev-parse`, `cat-file` and `git archive` serve other content for the
/// pinned commit id. Every source is therefore read through a `file://` clone with
/// `transfer.fsckObjects=true`, which refuses the replaced object.
#[test]
fn a_local_source_with_a_replaced_loose_object_is_refused() {
    let (tmp, home, src, sha) = refusal_setup("tampered-object");
    let path = format!("{sha}:scripts/bootstrap.sh");
    let blob = git(&src, &["rev-parse", &path]);
    let other = tmp.path().join("other");
    fs::write(&other, "# tampered\n").unwrap();
    let oid = git(&src, &["hash-object", "-w", other.to_str().unwrap()]);
    let obj = |id: &str| src.join(format!(".git/objects/{}/{}", &id[..2], &id[2..]));
    fs::set_permissions(obj(&blob), fs::Permissions::from_mode(0o644)).unwrap();
    fs::copy(obj(&oid), obj(&blob)).unwrap();
    assert_eq!(
        git(
            &src,
            &["rev-parse", "--verify", &format!("{sha}^{{commit}}")]
        ),
        sha
    );
    assert_eq!(
        git(&src, &["cat-file", "blob", &path]),
        "# tampered",
        "git must serve the replaced content for this test to mean anything"
    );
    let before = snapshot(&home);
    let o = install_tiny(&home, &src, &sha);
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("E_SOURCE_UNTRUSTED") && err(&o).contains("cannot clone"),
        "{}",
        err(&o)
    );
    assert_eq!(
        snapshot(&home),
        before,
        "a refused clone must leave nothing"
    );
}

/// install resets the build directory, so a script or a local source that lives inside it would
/// be removed mid-operation; both are refused before anything is reset.
#[test]
fn a_script_or_source_inside_the_build_directory_is_refused_before_the_reset() {
    let (_tmp, home, src, sha) = refusal_setup("inside-build");
    let build = home.join(".cache/dotfiles-manager/build");
    fs::create_dir_all(&build).unwrap();
    let copy = build.join("bootstrap.sh");
    fs::copy(repo_root().join("scripts/bootstrap.sh"), &copy).unwrap();
    let o = script_file(
        &copy,
        &home,
        &[
            "install",
            "--source",
            src.to_str().unwrap(),
            "--commit",
            &sha,
        ],
        &[
            ("DFM_BOOTSTRAP_TEST", "1"),
            ("DFM_TEST_HOME", home.to_str().unwrap()),
        ],
    );
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("E_PREREQ_MISSING") && err(&o).contains("inside the build directory"),
        "{}",
        err(&o)
    );
    assert!(copy.is_file(), "the running script was removed");
    let inside = build.join("src");
    fs::create_dir_all(&inside).unwrap();
    // The path itself, a `..` route, a symlinked directory, a symlink leaf and a file:// form all
    // resolve to it.
    let link = _tmp.path().join("link");
    std::os::unix::fs::symlink(&build, &link).unwrap();
    let leaf = _tmp.path().join("leaf");
    std::os::unix::fs::symlink(&inside, &leaf).unwrap();
    for route in [
        inside.clone(),
        build.join("../build/src"),
        link.join("src"),
        leaf,
        PathBuf::from(format!("file://{}", inside.display())),
    ] {
        let o = install_tiny(&home, &route, &sha);
        assert_eq!(o.status.code(), Some(3), "{}: {}", route.display(), err(&o));
        assert!(
            err(&o).contains("inside the build directory"),
            "{}: {}",
            route.display(),
            err(&o)
        );
        assert!(
            inside.is_dir(),
            "{}: the source was removed",
            route.display()
        );
    }
    // The script reached through a symlink leaf outside the build directory.
    let script_link = _tmp.path().join("bootstrap.sh");
    std::os::unix::fs::symlink(&copy, &script_link).unwrap();
    let o = script_file(
        &script_link,
        &home,
        &[
            "install",
            "--source",
            src.to_str().unwrap(),
            "--commit",
            &sha,
        ],
        &[
            ("DFM_BOOTSTRAP_TEST", "1"),
            ("DFM_TEST_HOME", home.to_str().unwrap()),
        ],
    );
    assert_eq!(o.status.code(), Some(3), "{}", err(&o));
    assert!(
        err(&o).contains("inside the build directory"),
        "{}",
        err(&o)
    );
    assert!(copy.is_file(), "the running script was removed");
}
