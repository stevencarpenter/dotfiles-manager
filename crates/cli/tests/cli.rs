//! Installed-binary behavior in an isolated HOME: version, help, binding, refusals.

mod common;

use common::{TempDir, repo_root};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_dotfiles-manager");

fn cmd(home: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(BIN);
    c.env_clear().arg("--home").arg(home).args(args);
    c
}

fn run(home: &Path, args: &[&str]) -> Output {
    cmd(home, args).output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn profile(name: &str) -> String {
    repo_root()
        .join("examples/profiles")
        .join(name)
        .to_str()
        .unwrap()
        .to_string()
}

fn files_below(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd {
            let p = e.unwrap().path();
            out.push(p.clone());
            if p.is_dir() {
                out.extend(files_below(&p));
            }
        }
    }
    out
}

#[test]
fn version_and_help_need_no_home() {
    for flag in ["--version", "version"] {
        let o = Command::new(BIN).env_clear().arg(flag).output().unwrap();
        assert_eq!(o.status.code(), Some(0));
        let out = text(&o.stdout);
        assert!(
            out.starts_with("dotfiles-manager 0.1.0 (commit unknown)"),
            "{out}"
        );
    }
    for flag in ["--help", "help", "-h"] {
        let o = Command::new(BIN).env_clear().arg(flag).output().unwrap();
        assert_eq!(o.status.code(), Some(0));
        assert!(text(&o.stdout).contains("exit codes: 0 ok, 1 failed, 2 usage, 3 blocked"));
    }
}

#[test]
fn usage_errors_exit_2_with_one_bounded_line() {
    let home = TempDir::new("usage");
    let long = "x".repeat(5000);
    for args in [
        vec!["frobnicate"],
        vec!["bind"],
        vec!["bind", "--profile"],
        vec!["version", "extra"],
        vec!["self-check"],
        vec!["bind", &long],
    ] {
        let o = run(home.path(), &args);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        let err = text(&o.stderr);
        assert!(err.starts_with("error: E_USAGE "), "{err}");
        assert_eq!(err.lines().count(), 1);
        assert!(err.len() < 600, "unbounded error: {}", err.len());
    }
    assert!(
        files_below(home.path()).is_empty(),
        "usage errors must not touch HOME"
    );
}

#[test]
fn no_implicit_profile_inference() {
    let home = TempDir::new("noinfer");
    let o = run(home.path(), &["bind", "--machine-id", "m5-personal"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(text(&o.stderr).contains("--profile is required"));
    let o = run(home.path(), &["binding"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(text(&o.stdout).trim_end().ends_with("unbound"));
    assert!(files_below(home.path()).is_empty());
}

#[test]
fn bind_persists_with_private_modes_and_is_idempotent() {
    let home = TempDir::new("bind");
    let p = profile("personal-dev-01.toml");
    let o = run(
        home.path(),
        &["bind", "--profile", &p, "--machine-id", "m5-personal"],
    );
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert!(text(&o.stdout).contains("isolated: true"));
    let file = home.path().join(".config/dotfiles-manager/machine.toml");
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(file.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let before = fs::read(&file).unwrap();
    assert!(text(&before).contains("schema = 1"));
    let o = run(home.path(), &["binding"]);
    assert!(text(&o.stdout).contains(&format!("bound profile={p} machine_id=m5-personal")));
    let o = run(
        home.path(),
        &["bind", "--profile", &p, "--machine-id", "m5-personal"],
    );
    assert_eq!(o.status.code(), Some(0));
    assert!(text(&o.stdout).contains("already bound"));
    assert_eq!(fs::read(&file).unwrap(), before);
    let leftovers: Vec<_> = files_below(file.parent().unwrap())
        .into_iter()
        .filter(|p| p != &file)
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary files left behind: {leftovers:?}"
    );
}

#[test]
fn deliberate_rebinding_is_refused_and_binding_is_unchanged() {
    let home = TempDir::new("rebind");
    let (a, b) = (profile("personal-dev-01.toml"), profile("work-01.toml"));
    assert_eq!(
        run(
            home.path(),
            &["bind", "--profile", &a, "--machine-id", "m5-personal"]
        )
        .status
        .code(),
        Some(0)
    );
    let file = home.path().join(".config/dotfiles-manager/machine.toml");
    let before = fs::read(&file).unwrap();
    for args in [
        ["bind", "--profile", &b, "--machine-id", "m5-personal"],
        ["bind", "--profile", &a, "--machine-id", "m3-work"],
        ["bind", "--profile", &b, "--machine-id", "m3-work"],
    ] {
        let o = run(home.path(), &args);
        assert_eq!(o.status.code(), Some(3), "{args:?}");
        assert!(
            text(&o.stderr).contains("E_REBIND_UNSUPPORTED"),
            "{}",
            text(&o.stderr)
        );
    }
    assert!(
        run(
            home.path(),
            &[
                "bind",
                "--force",
                "--profile",
                &b,
                "--machine-id",
                "m3-work"
            ]
        )
        .status
        .code()
            == Some(2),
        "no force flag exists"
    );
    assert_eq!(fs::read(&file).unwrap(), before);
}

#[test]
fn unsupported_platform_is_refused_before_mutation() {
    let home = TempDir::new("platform");
    let p = profile("personal-dev-01.toml");
    for plat in ["linux/aarch64", "macos/x86_64", "linux/x86_64", "windows"] {
        let o = cmd(
            home.path(),
            &["bind", "--profile", &p, "--machine-id", "m5"],
        )
        .env("DFM_PLATFORM", plat)
        .output()
        .unwrap();
        assert_eq!(o.status.code(), Some(3), "{plat}");
        assert!(
            text(&o.stderr).contains("E_PLATFORM_UNSUPPORTED"),
            "{}",
            text(&o.stderr)
        );
    }
    assert!(
        files_below(home.path()).is_empty(),
        "refusal must precede any write"
    );
}

#[test]
fn invalid_profile_and_machine_id_are_refused_before_mutation() {
    let home = TempDir::new("invalid");
    let o = run(
        home.path(),
        &[
            "bind",
            "--profile",
            "/nonexistent/profile.toml",
            "--machine-id",
            "m5",
        ],
    );
    assert_eq!(
        (
            o.status.code(),
            text(&o.stderr).contains("E_PROFILE_INVALID")
        ),
        (Some(3), true)
    );
    let o = run(
        home.path(),
        &[
            "bind",
            "--profile",
            home.path().to_str().unwrap(),
            "--machine-id",
            "m5",
        ],
    );
    assert_eq!(o.status.code(), Some(3));
    let o = run(
        home.path(),
        &[
            "bind",
            "--profile",
            &profile("work-01.toml"),
            "--machine-id",
            "Bad Id",
        ],
    );
    assert_eq!(o.status.code(), Some(2));
    assert!(files_below(home.path()).is_empty());
}

#[test]
fn damaged_or_newer_binding_is_never_overwritten() {
    let home = TempDir::new("damaged");
    let file = home.path().join(".config/dotfiles-manager/machine.toml");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    let p = profile("work-01.toml");
    for (content, code) in [
        ("schema = 2\n", "E_SCHEMA_UNSUPPORTED"),
        ("garbage [", "E_BINDING_INVALID"),
        ("schema = 1\nextra = 1\n", "E_BINDING_INVALID"),
    ] {
        fs::write(&file, content).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        for args in [
            vec!["bind", "--profile", &p, "--machine-id", "m3"],
            vec!["binding"],
        ] {
            let o = run(home.path(), &args);
            assert_eq!(o.status.code(), Some(3), "{content}");
            assert!(text(&o.stderr).contains(code), "{}", text(&o.stderr));
        }
        assert_eq!(fs::read_to_string(&file).unwrap(), content);
    }
}

#[test]
fn concurrent_binds_leave_exactly_one_identity() {
    let (a, b) = (profile("personal-dev-01.toml"), profile("work-01.toml"));
    for round in 0..3 {
        let home = TempDir::new("race");
        let kids: Vec<_> = (0..8)
            .map(|i| {
                let p = if i % 2 == 0 { &a } else { &b };
                let c = cmd(home.path(), &["bind", "--profile", p, "--machine-id", "m5"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                (p.clone(), c)
            })
            .collect();
        let results: Vec<_> = kids
            .into_iter()
            .map(|(p, c)| (p, c.wait_with_output().unwrap()))
            .collect();
        let bound = text(&run(home.path(), &["binding"]).stdout);
        let winners: Vec<_> = results
            .iter()
            .filter(|(_, o)| o.status.code() == Some(0))
            .collect();
        assert!(!winners.is_empty(), "round {round}: nobody bound");
        for (p, o) in &results {
            match o.status.code() {
                Some(0) => assert!(
                    bound.contains(&format!("profile={p} ")),
                    "round {round}: success for a different identity"
                ),
                // The loser either met the winner's binding (another identity) or the manager
                // lock (docs/architecture.md section 10: a second writer exits 3 E_LOCKED).
                Some(3) => assert!(
                    text(&o.stderr).contains("E_REBIND_UNSUPPORTED")
                        || text(&o.stderr).contains("E_LOCKED"),
                    "{}",
                    text(&o.stderr)
                ),
                other => panic!("round {round}: unexpected exit {other:?}"),
            }
        }
        let leftovers: Vec<_> = files_below(&home.path().join(".config/dotfiles-manager"))
            .into_iter()
            .filter(|p| !p.ends_with("machine.toml"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}

#[test]
fn home_must_match_the_password_database() {
    // No override: detection runs. A temporary HOME cannot equal the real password database home.
    let home = TempDir::new("mismatch");
    let o = Command::new(BIN)
        .env_clear()
        .env("HOME", home.path())
        .arg("binding")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(3));
    assert!(
        text(&o.stderr).contains("E_HOME_MISMATCH"),
        "{}",
        text(&o.stderr)
    );
    assert!(files_below(home.path()).is_empty());
}

#[test]
fn self_check_fixture_dir_reads_only_that_directory() {
    let home = TempDir::new("fixture");
    let dir = TempDir::new("fixture-dir");
    assert_eq!(
        run(
            home.path(),
            &["self-check", "--fixture-dir", dir.path().to_str().unwrap()]
        )
        .status
        .code(),
        Some(0)
    );
    fs::write(dir.path().join("machine.toml"), "schema = 9\n").unwrap();
    let o = run(
        home.path(),
        &["self-check", "--fixture-dir", dir.path().to_str().unwrap()],
    );
    assert_eq!(o.status.code(), Some(3));
    assert_eq!(
        run(
            home.path(),
            &["self-check", "--fixture-dir", "/nonexistent"]
        )
        .status
        .code(),
        Some(3)
    );
    assert!(files_below(home.path()).is_empty());
}

#[test]
fn a_home_with_whitespace_is_refused() {
    let tmp = TempDir::new("space");
    let home = tmp.path().join("a b");
    fs::create_dir(&home).unwrap();
    let o = run(&home, &["binding"]);
    assert_eq!(o.status.code(), Some(3));
    assert!(
        text(&o.stderr).contains("E_HOME_MISMATCH"),
        "{}",
        text(&o.stderr)
    );
    assert!(files_below(&home).is_empty());
}

#[test]
fn a_profile_must_be_a_toml_file_with_a_schema_key() {
    let home = TempDir::new("profile-content");
    let dir = TempDir::new("profile-files");
    for (name, content) in [
        ("empty.toml", ""),
        ("not-toml.toml", "this is [not toml"),
        ("no-schema.toml", "id = \"x\"\n"),
        ("nested-schema.toml", "[t]\nschema = 1\n"),
    ] {
        let f = dir.path().join(name);
        fs::write(&f, content).unwrap();
        let o = run(
            home.path(),
            &[
                "bind",
                "--profile",
                f.to_str().unwrap(),
                "--machine-id",
                "m5",
            ],
        );
        assert_eq!(o.status.code(), Some(3), "{name}");
        assert!(
            text(&o.stderr).contains("E_PROFILE_INVALID"),
            "{name}: {}",
            text(&o.stderr)
        );
    }
    assert!(files_below(home.path()).is_empty());
}

#[test]
fn a_binding_that_is_a_symlink_or_not_private_is_refused() {
    let home = TempDir::new("binding-kind");
    let p = profile("work-01.toml");
    assert_eq!(
        run(
            home.path(),
            &["bind", "--profile", &p, "--machine-id", "m3"]
        )
        .status
        .code(),
        Some(0)
    );
    let file = home.path().join(".config/dotfiles-manager/machine.toml");
    let good = fs::read(&file).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    for args in [
        vec!["binding"],
        vec!["bind", "--profile", &p, "--machine-id", "m3"],
    ] {
        let o = run(home.path(), &args);
        assert_eq!(o.status.code(), Some(3), "{args:?}");
        assert!(
            text(&o.stderr).contains("E_BINDING_INVALID") && text(&o.stderr).contains("0600"),
            "{}",
            text(&o.stderr)
        );
    }
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    let elsewhere = home.path().join("elsewhere.toml");
    fs::write(&elsewhere, &good).unwrap();
    fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o600)).unwrap();
    fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &file).unwrap();
    let o = run(home.path(), &["binding"]);
    assert_eq!(o.status.code(), Some(3));
    assert!(
        text(&o.stderr).contains("E_BINDING_INVALID"),
        "{}",
        text(&o.stderr)
    );
}

#[test]
fn an_io_failure_while_writing_the_binding_exits_1() {
    let home = TempDir::new("io-failure");
    let cfg = home.path().join(".config/dotfiles-manager");
    fs::create_dir_all(&cfg).unwrap();
    fs::set_permissions(&cfg, fs::Permissions::from_mode(0o500)).unwrap();
    let o = run(
        home.path(),
        &[
            "bind",
            "--profile",
            &profile("work-01.toml"),
            "--machine-id",
            "m3",
        ],
    );
    fs::set_permissions(&cfg, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("E_IO_FAILED"),
        "{}",
        text(&o.stderr)
    );
    assert!(!cfg.join("machine.toml").exists());
}

#[test]
fn a_fifo_at_the_binding_path_is_refused_without_blocking() {
    let home = TempDir::new("fifo");
    let cfg = home.path().join(".config/dotfiles-manager");
    fs::create_dir_all(&cfg).unwrap();
    assert!(
        Command::new("/usr/bin/mkfifo")
            .arg(cfg.join("machine.toml"))
            .status()
            .unwrap()
            .success()
    );
    for args in [
        vec!["binding"],
        vec![
            "bind",
            "--profile",
            &profile("work-01.toml"),
            "--machine-id",
            "m3",
        ],
    ] {
        let mut child = cmd(home.path(), &args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            if std::time::Instant::now() > deadline {
                child.kill().unwrap();
                panic!("{args:?} blocked on a FIFO");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert_eq!(status.code(), Some(3), "{args:?}");
    }
}

#[test]
fn a_restrictive_umask_still_produces_a_binding_the_reader_accepts() {
    let home = TempDir::new("umask");
    let cfg = home.path().join(".config/dotfiles-manager");
    fs::create_dir_all(&cfg).unwrap();
    fs::set_permissions(&cfg, fs::Permissions::from_mode(0o700)).unwrap();
    let p = profile("work-01.toml");
    // umask 277 would turn the creation mode 0600 into 0400.
    let o = Command::new("/bin/sh")
        .args(["-c", "umask 277; exec \"$0\" \"$@\"", BIN, "--home"])
        .arg(home.path())
        .args(["bind", "--profile", &p, "--machine-id", "m3"])
        .env_clear()
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let file = cfg.join("machine.toml");
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(run(home.path(), &["binding"]).status.code(), Some(0));
}
