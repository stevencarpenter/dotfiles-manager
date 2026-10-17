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
    Runner {
        search_path: vec![bin.to_path_buf()],
        home: bin.to_path_buf(),
        cwd: bin.join("run"),
        uid: std::fs::metadata(bin).unwrap().uid(),
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
    assert!(d.join("bin/run").is_dir());
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
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn output_over_the_cap_is_refused_and_stderr_is_bounded() {
    let d = scratch("cap");
    script(
        &d.join("bin"),
        "tool",
        "head -c 10000 /dev/zero | tr '\\0' e >&2; head -c 20 /dev/zero",
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
