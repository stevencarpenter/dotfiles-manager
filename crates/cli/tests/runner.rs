//! The bounded child-process runner (`dfm_engine::proc`, docs/secrets.md "How `op` is run"),
//! tested from the CLI crate because its scripts name system tools, which the engine's sources
//! may not (gate test `os_bound_names_stay_in_platform_macos`).

use dfm_engine::proc::{MAX_STDERR, passthrough};
use dfm_engine::{End, ProcRequest as Request, Runner};
use dfm_model::Code;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn runner(bin: &Path) -> Runner {
    // The commands that start a provider install the handlers first, so the child gets its own
    // process group. Installing them in the test process changes nothing else here.
    dfm_engine::signal::install().unwrap();
    Runner {
        search_path: vec![bin.to_path_buf()],
        home: bin.to_path_buf(),
        cwd: bin.join("run"),
        uid: std::fs::metadata(bin).unwrap().uid(),
        grace: Duration::from_secs(1),
    }
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    // The runner gives the child only the search path, so the script names the system tools.
    std::fs::write(&p, format!("#!/bin/sh\nPATH=/usr/bin:/bin:$PATH\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn scratch(label: &str) -> PathBuf {
    let d = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("dfm-proc-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("bin")).unwrap();
    d
}

fn req<'a>(args: &'a [String], stdin: Option<&'a [u8]>, secs: u64) -> Request<'a> {
    Request {
        program: "tool",
        args,
        env: &[],
        stdin,
        timeout: Duration::from_secs(secs),
        max_stdout: 1 << 20,
    }
}

#[test]
fn the_environment_is_rebuilt_and_stdin_reaches_the_child() {
    let d = scratch("env");
    script(
        &d.join("bin"),
        "tool",
        "echo \"path=$PATH home=$HOME lang=$LANG user=${USER:-none} tmp=${TMPDIR:-none}\"; cat",
    );
    let r = runner(&d.join("bin"));
    let out = r.run(&req(&[], Some(b"template"), 5)).unwrap();
    assert!(out.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    // The test process has USER and TMPDIR; the child must not.
    assert!(text.contains("user=none tmp=none"), "{text}");
    assert!(
        text.contains(&format!(":{} home=", d.join("bin").display())),
        "{text}"
    );
    assert!(
        text.contains(&format!("home={}", d.join("bin").display())),
        "{text}"
    );
    assert!(text.contains("lang=C"), "{text}");
    assert!(text.ends_with("template"), "{text}");
    assert!(out.stdin_written);
    assert!(d.join("bin/run").is_dir());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_process_that_keeps_the_pipes_open_is_ended_and_run_returns_within_the_bound() {
    let d = scratch("linger");
    // The background sleep inherits stdout and would hold it for 20 s.
    script(&d.join("bin"), "tool", "(sleep 20 &) ; exit 0");
    let r = runner(&d.join("bin"));
    let t = Instant::now();
    let out = r.run(&req(&[], None, 5)).unwrap();
    assert!(matches!(out.end, End::Exited(0)), "{out:?}");
    assert!(t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
    // A member that ignores SIGTERM gets SIGKILL after the grace period.
    script(
        &d.join("bin"),
        "tool",
        "(trap '' TERM; exec sleep 20) & sleep 20",
    );
    let t = Instant::now();
    let out = r.run(&req(&[], None, 1)).unwrap();
    assert!(matches!(out.end, End::TimedOut), "{out:?}");
    assert!(t.elapsed() < Duration::from_secs(6), "{:?}", t.elapsed());
    // A child that closed its pipes but ignores SIGTERM gets SIGKILL too: run does not wait on it.
    script(
        &d.join("bin"),
        "tool",
        "trap '' TERM; exec >/dev/null 2>&1; sleep 20",
    );
    let t = Instant::now();
    let out = r.run(&req(&[], None, 1)).unwrap();
    assert!(matches!(out.end, End::TimedOut), "{out:?}");
    assert!(t.elapsed() < Duration::from_secs(6), "{:?}", t.elapsed());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_process_that_holds_stdin_without_reading_is_ended_before_run_returns() {
    let d = scratch("stdin");
    // A descendant keeps standard input and never reads it; the child closes its output and
    // exits. The writer of a template larger than the pipe buffer stays blocked until the
    // descendant is gone, so run must end it rather than return around it.
    let pid = d.join("pid");
    script(
        &d.join("bin"),
        "tool",
        &format!(
            "sleep 20 <&0 >/dev/null 2>&1 & echo $! > {}; exit 0",
            pid.display()
        ),
    );
    let r = runner(&d.join("bin"));
    let big = vec![b'x'; 1 << 20];
    let t = Instant::now();
    let out = r.run(&req(&[], Some(&big), 5)).unwrap();
    assert!(matches!(out.end, End::Exited(0)), "{out:?}");
    assert!(t.elapsed() < Duration::from_secs(6), "{:?}", t.elapsed());
    let pid = std::fs::read_to_string(&pid).unwrap();
    let alive = std::process::Command::new("/bin/kill")
        .args(["-0", pid.trim()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(!alive, "the descendant {} outlived run", pid.trim());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_timeout_ends_the_child_and_reports_it() {
    let d = scratch("timeout");
    script(&d.join("bin"), "tool", "sleep 30; echo late");
    let r = runner(&d.join("bin"));
    let t = Instant::now();
    let out = r.run(&req(&[], None, 1)).unwrap();
    assert!(matches!(out.end, End::TimedOut), "{out:?}");
    assert!(t.elapsed() < Duration::from_secs(15), "{:?}", t.elapsed());
    assert!(out.stdout.is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_program_that_fails_its_checks_does_not_run() {
    let d = scratch("checks");
    let p = script(&d.join("bin"), "tool", "echo ran > ran");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o777)).unwrap();
    let r = runner(&d.join("bin"));
    let e = r.run(&req(&[], None, 5)).unwrap_err();
    assert_eq!(e.code, Code::ProbeFailed);
    assert!(e.detail.contains("writable"), "{e}");
    assert!(!d.join("bin/run/ran").exists());
    for bad in ["", "-x", "a/b"] {
        assert_eq!(r.resolve(bad).unwrap_err().code, Code::ProbeFailed);
    }
    assert!(
        r.resolve("missing")
            .unwrap_err()
            .detail
            .contains("not found")
    );
    // An earlier directory where the name cannot be looked up is refused, not skipped: a later
    // directory must not supply the program.
    let closed = d.join("closed");
    std::fs::create_dir(&closed).unwrap();
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut r2 = runner(&d.join("bin"));
    r2.search_path.insert(0, closed.clone());
    let e = r2.resolve("tool").unwrap_err();
    assert_eq!(e.code, Code::ProbeFailed);
    assert!(e.detail.contains("closed/tool"), "{e}");
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn output_over_the_cap_is_refused_and_stderr_is_bounded() {
    let d = scratch("cap");
    script(
        &d.join("bin"),
        "tool",
        "head -c 40000000 /dev/zero | tr '\\0' e >&2; head -c 20 /dev/zero",
    );
    let r = runner(&d.join("bin"));
    let out = r
        .run(&Request {
            max_stdout: 20,
            ..req(&[], None, 5)
        })
        .unwrap();
    assert_eq!(out.stdout.len(), 20);
    assert_eq!(out.stderr.len(), MAX_STDERR);
    let e = r
        .run(&Request {
            max_stdout: 19,
            ..req(&[], None, 5)
        })
        .unwrap_err();
    assert_eq!(e.code, Code::LimitExceeded);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn passthrough_takes_only_the_named_prefixes() {
    // The test process has PATH and HOME. Only the named prefix comes through.
    let got = passthrough(&["PAT"]);
    assert!(!got.is_empty());
    assert!(got.iter().all(|(k, _)| k.starts_with("PAT")), "{got:?}");
    assert!(passthrough(&["NO_SUCH_PREFIX_"]).is_empty());
}

#[test]
fn every_resolve_check_refuses_and_a_symlinked_search_dir_is_compared_canonically() {
    let d = scratch("resolve");
    let bin = d.join("bin");
    let refuses = |r: &Runner, want: &str| {
        let e = r.resolve("tool").unwrap_err();
        assert_eq!(e.code, Code::ProbeFailed, "{e}");
        assert!(e.detail.contains(want), "{want}: {e}");
    };
    let r = runner(&bin);
    // Not executable.
    let p = script(&bin, "tool", "exit 0");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    refuses(&r, "is not executable");
    // Owned by another user than root or the runner's user.
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    let other = Runner {
        uid: r.uid + 1,
        ..r.clone()
    };
    refuses(&other, "owned by another user");
    // Not a regular file.
    std::fs::remove_file(&p).unwrap();
    std::fs::create_dir(&p).unwrap();
    refuses(&r, "is not a regular file");
    std::fs::remove_dir(&p).unwrap();
    // A link that leaves the search directory's prefix.
    let outside = scratch("resolve-outside");
    let target = script(&outside.join("bin"), "tool", "exit 0");
    std::os::unix::fs::symlink(&target, &p).unwrap();
    refuses(&r, "outside");
    std::fs::remove_file(&p).unwrap();
    // A search directory reached through a symlink elsewhere is compared by its canonical path.
    let real = script(&bin, "tool", "exit 0");
    let alias_root = scratch("resolve-alias");
    let alias = alias_root.join("alias");
    std::os::unix::fs::symlink(&bin, &alias).unwrap();
    let via = Runner {
        search_path: vec![alias],
        ..r.clone()
    };
    assert_eq!(via.resolve("tool").unwrap(), real.canonicalize().unwrap());
    for x in [&d, &outside, &alias_root] {
        let _ = std::fs::remove_dir_all(x);
    }
}

#[test]
fn a_working_directory_that_is_not_a_plain_directory_and_a_nul_argument_are_refused() {
    let d = scratch("cwd");
    script(&d.join("bin"), "tool", "echo ran > ran");
    let r = runner(&d.join("bin"));
    let elsewhere = d.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let link = d.join("cwd-link");
    std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
    let file = d.join("cwd-file");
    std::fs::write(&file, "").unwrap();
    for cwd in [link, file] {
        let bad = Runner { cwd, ..r.clone() };
        let e = bad.run(&req(&[], None, 5)).unwrap_err();
        assert!(e.detail.contains("not a plain directory"), "{e}");
    }
    assert!(!elsewhere.join("ran").exists());
    let e = r.run(&req(&["a\0b".to_string()], None, 5)).unwrap_err();
    assert!(e.detail.contains("NUL byte"), "{e}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_child_that_exits_before_reading_all_its_input_is_reported() {
    let d = scratch("partial");
    script(&d.join("bin"), "tool", "exit 0");
    let r = runner(&d.join("bin"));
    let big = vec![b'x'; 1 << 20];
    let out = r.run(&req(&[], Some(&big), 5)).unwrap();
    assert!(out.success(), "{out:?}");
    assert!(!out.stdin_written, "{out:?}");
    let _ = std::fs::remove_dir_all(&d);
}
