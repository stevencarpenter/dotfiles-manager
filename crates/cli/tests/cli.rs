//! Installed-binary behavior in an isolated HOME: version, help, binding, refusals.

mod common;

use common::{TempDir, repo_root};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_dfm");

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

// The checkout that holds examples/profiles. A pushed commit is tested in a tree without .git,
// so a bind of those profiles names it rather than relying on the .git default.
fn checkout() -> String {
    repo_root().to_str().unwrap().to_string()
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
        assert!(out.starts_with("dfm 0.1.0 (commit unknown)"), "{out}");
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
    let (p, root) = (profile("personal-dev-01.toml"), checkout());
    let o = run(
        home.path(),
        &[
            "bind",
            "--profile",
            &p,
            "--machine-id",
            "m5-personal",
            "--root",
            &root,
        ],
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
    assert!(text(&before).contains("schema = 2"));
    let o = run(home.path(), &["binding"]);
    assert!(text(&o.stdout).contains(&format!("bound profile={p} machine_id=m5-personal")));
    let o = run(
        home.path(),
        &[
            "bind",
            "--profile",
            &p,
            "--machine-id",
            "m5-personal",
            "--root",
            &root,
        ],
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
    let root = checkout();
    assert_eq!(
        run(
            home.path(),
            &[
                "bind",
                "--profile",
                &a,
                "--machine-id",
                "m5-personal",
                "--root",
                &root
            ]
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
    assert!(
        text(&o.stderr).contains("E_PROFILE_INVALID"),
        "{}",
        text(&o.stderr)
    );
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
        ("schema = 3\n", "E_SCHEMA_UNSUPPORTED"),
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
    let root = checkout();
    for round in 0..3 {
        let home = TempDir::new("race");
        let kids: Vec<_> = (0..8)
            .map(|i| {
                let p = if i % 2 == 0 { &a } else { &b };
                let args = [
                    "bind",
                    "--profile",
                    p,
                    "--machine-id",
                    "m5",
                    "--root",
                    &root,
                ];
                let c = cmd(home.path(), &args)
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
                // Exit 1 is the same refusal from a loser that had already created a shared
                // directory (such as .local/state) before it met the lock: a mutation began.
                Some(3 | 1) => assert!(
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
    assert!(
        text(&o.stderr).contains("E_BINDING_INVALID"),
        "{}",
        text(&o.stderr)
    );
    let missing = run(
        home.path(),
        &["self-check", "--fixture-dir", "/nonexistent"],
    );
    assert_eq!(missing.status.code(), Some(3));
    assert!(
        text(&missing.stderr).contains("E_BINDING_INVALID"),
        "{}",
        text(&missing.stderr)
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
    for (name, content, want) in [
        ("empty.toml", "", "E_PROFILE_INVALID"),
        ("not-toml.toml", "this is [not toml", "E_PROFILE_INVALID"),
        ("no-schema.toml", "id = \"x\"\n", "E_PROFILE_INVALID"),
        (
            "nested-schema.toml",
            "[t]\nschema = 1\n",
            "E_PROFILE_INVALID",
        ),
        ("schema-2.toml", "schema = 2\n", "E_SCHEMA_UNSUPPORTED"),
        (
            "schema-string.toml",
            "schema = \"1\"\n",
            "E_PROFILE_INVALID",
        ),
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
            text(&o.stderr).contains(want),
            "{name}: {}",
            text(&o.stderr)
        );
    }
    assert!(files_below(home.path()).is_empty());
}

#[test]
fn a_binding_that_is_a_symlink_or_not_private_is_refused() {
    let home = TempDir::new("binding-kind");
    let (p, root) = (profile("work-01.toml"), checkout());
    assert_eq!(
        run(
            home.path(),
            &[
                "bind",
                "--profile",
                &p,
                "--machine-id",
                "m3",
                "--root",
                &root
            ]
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
            "--root",
            &checkout(),
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
        let mut said = String::new();
        std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut said).unwrap();
        assert!(said.contains("E_BINDING_INVALID"), "{args:?}: {said}");
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
        .args(["bind", "--profile", &p, "--machine-id", "m3", "--root"])
        .arg(checkout())
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

#[test]
fn bind_binding_and_state_escape_an_invisible_profile_path() {
    let home = TempDir::new("escape-home");
    let src = TempDir::new("escape-src");
    let dir = src.path().join("p\u{202e}q");
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join("profile.toml");
    fs::write(&p, "schema = 1\n").unwrap();
    let p = p.to_str().unwrap();
    let r = dir.to_str().unwrap();
    let outputs = [
        run(
            home.path(),
            &["bind", "--profile", p, "--machine-id", "m5", "--root", r],
        ),
        run(home.path(), &["binding"]),
        run(home.path(), &["state", "check"]),
    ];
    for o in outputs {
        let out = text(&o.stdout);
        assert_eq!(o.status.code(), Some(0), "{out}{}", text(&o.stderr));
        assert!(!out.contains('\u{202e}'), "raw bidi control in: {out}");
    }
    assert!(text(&run(home.path(), &["binding"]).stdout).contains("p\\u{202e}q"));
}

#[test]
fn extra_arguments_to_a_no_argument_command_say_so() {
    let home = TempDir::new("cli-noargs");
    for args in [["binding", "--json"], ["help", "x"], ["version", "x"]] {
        let o = run(home.path(), &args);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        let err = text(&o.stderr);
        assert!(
            err.contains(&format!("{} takes no arguments", args[0])),
            "{err}"
        );
    }
    let o = run(home.path(), &["nonesuch"]);
    assert!(text(&o.stderr).contains("unknown command or argument nonesuch"));
}

// ---- DFM-61: the checkout root in the binding ---------------------------------------------

fn binding_file(fx: &common::Fx) -> PathBuf {
    fx.h(".config/dotfiles-manager/machine.toml")
}

/// Rewrites the binding in the schema 1 form, written before the root was recorded.
fn as_schema_1(fx: &common::Fx) {
    let f = binding_file(fx);
    let body = fs::read_to_string(&f).unwrap();
    let old: String = body
        .lines()
        .filter(|l| !l.starts_with("root = "))
        .map(|l| format!("{l}\n"))
        .collect();
    fs::write(&f, old.replace("schema = 2", "schema = 1")).unwrap();
}

#[test]
fn every_command_defaults_to_the_recorded_root_and_an_explicit_root_wins() {
    let fx = common::Fx::new();
    let real = fs::canonicalize(&fx.root).unwrap();
    let body = fs::read_to_string(binding_file(&fx)).unwrap();
    assert!(
        body.contains(&format!("root = \"{}\"", real.display())),
        "{body}"
    );
    // The same tree gives the same plan and digest from the flag or from the binding.
    let with = fx.run(&["plan", "--root", fx.root_arg()]);
    let without = fx.run(&["plan"]);
    assert_eq!(common::code(&without), 0, "{}", text(&without.stderr));
    assert!(text(&without.stdout).contains("plan digest: "));
    assert_eq!(text(&with.stdout), text(&without.stdout));
    for args in [
        vec!["explain", "extra.zsh"],
        vec!["doctor"],
        vec!["status"],
        vec!["apply", "--preview"],
        vec!["secrets", "--preview"],
    ] {
        let o = fx.run(&args);
        assert_eq!(common::code(&o), 0, "{args:?}: {}", text(&o.stderr));
    }
    let o = fx.run(&["plan", "--root", "/nonexistent-dfm-root"]);
    assert_eq!(common::code(&o), 2);
    assert!(text(&o.stderr).contains("E_REPORT_UNAVAILABLE"));
}

#[test]
fn a_schema_1_binding_needs_root_until_the_same_bind_records_it_in_place() {
    let fx = common::Fx::new();
    as_schema_1(&fx);
    for args in [
        vec!["plan"],
        vec!["explain", "extra.zsh"],
        vec!["doctor"],
        vec!["apply", "--preview"],
        vec!["sync", "--preview"],
    ] {
        let o = fx.run(&args);
        assert_eq!(common::code(&o), 2, "{args:?}: {}", text(&o.stderr));
        assert!(
            text(&o.stderr).contains("the binding records no root"),
            "{args:?}: {}",
            text(&o.stderr)
        );
    }
    let o = fx.run(&["plan", "--root", fx.root_arg()]);
    assert_eq!(common::code(&o), 0, "{}", text(&o.stderr));
    // No .git above the profile and no --root: bind names --root and writes nothing.
    let p = fx.root.join("profiles/t-01.toml");
    let p = p.to_str().unwrap();
    let file = binding_file(&fx);
    let before = fs::read(&file).unwrap();
    let o = fx.run(&["bind", "--profile", p, "--machine-id", "m5-test"]);
    assert_eq!(common::code(&o), 2, "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("--root"), "{}", text(&o.stderr));
    assert_eq!(fs::read(&file).unwrap(), before);
    // A --root that does not hold the profile is refused before anything is written.
    let o = fx.run(&[
        "bind",
        "--profile",
        p,
        "--machine-id",
        "m5-test",
        "--root",
        fx.home.path().to_str().unwrap(),
    ]);
    assert!(
        text(&o.stderr).contains("E_PROFILE_INVALID"),
        "{}",
        text(&o.stderr)
    );
    assert_eq!(fs::read(&file).unwrap(), before);
    let up = [
        "bind",
        "--profile",
        p,
        "--machine-id",
        "m5-test",
        "--root",
        fx.root_arg(),
    ];
    let journal =
        || fs::read_to_string(fx.h(".local/state/dotfiles-manager/state/journal.jsonl")).unwrap();
    // A failure before the rename keeps the schema 1 binding and closes the intent as failed.
    let o = fx.run_env(&[("DFM_FAILPOINT", "binding-after-tmp:error")], &up);
    assert_eq!(common::code(&o), 1, "{}", text(&o.stderr));
    assert_eq!(fs::read(&file).unwrap(), before);
    assert!(journal().contains(r#""phase":"failed""#), "{}", journal());
    let o = fx.run(&["state", "check"]);
    assert_eq!(common::code(&o), 0, "{}", text(&o.stdout));
    // The same identity with --root upgrades the binding in place and journals it.
    let o = fx.run(&up);
    assert_eq!(common::code(&o), 0, "{}", text(&o.stderr));
    assert!(
        text(&o.stdout).contains("recorded root"),
        "{}",
        text(&o.stdout)
    );
    let body = fs::read_to_string(&file).unwrap();
    assert!(
        body.contains("schema = 2") && body.contains("root = "),
        "{body}"
    );
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(journal().contains("binding-upgrade-"), "{}", journal());
    // The journal entries are settled outcomes: nothing reads them as open work.
    for args in [vec!["plan"], vec!["state", "check"], vec!["doctor"]] {
        let o = fx.run(&args);
        assert_eq!(common::code(&o), 0, "{args:?}: {}", text(&o.stderr));
    }
    let o = fx.run(&up);
    assert!(
        text(&o.stdout).contains("already bound"),
        "{}",
        text(&o.stdout)
    );
    // After the rename the new binding is in place even when its durability is unconfirmed: the
    // run fails (exit 1) and the journal records the upgrade as done.
    as_schema_1(&fx);
    let o = fx.run_env(&[("DFM_FAILPOINT", "binding-after-rename:error")], &up);
    assert_eq!(common::code(&o), 1, "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("E_IO_UNCONFIRMED"),
        "{}",
        text(&o.stderr)
    );
    assert!(fs::read_to_string(&file).unwrap().contains("schema = 2"));
    assert!(
        journal().matches(r#""phase":"done""#).count() >= 2,
        "{}",
        journal()
    );
    let o = fx.run(&["binding"]);
    assert!(text(&o.stdout).contains(" root="), "{}", text(&o.stdout));
    // A different identity is still a rebind.
    let o = fx.run(&[
        "bind",
        "--profile",
        p,
        "--machine-id",
        "m3",
        "--root",
        fx.root_arg(),
    ]);
    assert!(text(&o.stderr).contains("E_REBIND_UNSUPPORTED"));
}

#[test]
fn bind_records_the_checkout_above_the_profile_and_keeps_it() {
    let fx = common::Fx::unbound();
    let p = fx.root.join("profiles/t-01.toml");
    let p = p.to_str().unwrap();
    // A root that does not hold the profile is refused before anything is written.
    let o = fx.run(&[
        "bind",
        "--profile",
        p,
        "--machine-id",
        "m5-test",
        "--root",
        fx.home.path().to_str().unwrap(),
    ]);
    assert!(
        text(&o.stderr).contains("E_PROFILE_INVALID"),
        "{}",
        text(&o.stderr)
    );
    assert!(!binding_file(&fx).exists());
    // A worktree's .git is a file; it marks the checkout as a directory does.
    fs::write(fx.root.join(".git"), "gitdir: /elsewhere\n").unwrap();
    let o = fx.run(&["bind", "--profile", p, "--machine-id", "m5-test"]);
    assert_eq!(common::code(&o), 0, "{}", text(&o.stderr));
    let real = fs::canonicalize(&fx.root).unwrap();
    let body = fs::read_to_string(binding_file(&fx)).unwrap();
    assert!(
        body.contains(&format!("root = \"{}\"", real.display())),
        "{body}"
    );
    // The recorded root is kept: another --root for the same identity is refused.
    let other = fx.root.join("profiles");
    let o = fx.run(&[
        "bind",
        "--profile",
        p,
        "--machine-id",
        "m5-test",
        "--root",
        other.to_str().unwrap(),
    ]);
    assert!(
        text(&o.stderr).contains("E_REBIND_UNSUPPORTED"),
        "{}",
        text(&o.stderr)
    );
    assert_eq!(fs::read_to_string(binding_file(&fx)).unwrap(), body);
}
