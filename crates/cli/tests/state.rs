//! Durable state through the built binary in isolated HOME directories (DFM-17).
//!
//! Operations are recorded by the `__record-op` harness command, which exists only in the
//! `test-isolation` build. The binary under test is the same one the other CLI tests drive.

mod common;

use common::{TempDir, is_root, repo_root};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_dotfiles-manager");
const M5: &str = "m5-personal";

fn cmd(home: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(BIN);
    c.env_clear().arg("--home").arg(home).args(args);
    c
}

fn run(home: &Path, args: &[&str]) -> Output {
    cmd(home, args).output().unwrap()
}

fn with_fail(home: &Path, point: &str, args: &[&str]) -> Output {
    cmd(home, args)
        .env("DFM_FAILPOINT", point)
        .output()
        .unwrap()
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

fn bind(home: &Path) -> Output {
    run(
        home,
        &[
            "bind",
            "--profile",
            &profile("personal-dev-01.toml"),
            "--machine-id",
            M5,
        ],
    )
}

fn bound_home(label: &str) -> TempDir {
    let h = TempDir::new(label);
    let o = bind(h.path());
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    h
}

fn root(h: &Path) -> PathBuf {
    h.join(".local/state/dotfiles-manager")
}

fn state(h: &Path) -> PathBuf {
    root(h).join("state")
}

fn out_file(h: &Path, name: &str) -> String {
    h.join(name).to_str().unwrap().to_string()
}

fn record(h: &Path, resource: &str, name: &str, content: &str) -> Output {
    let out = out_file(h, name);
    run(
        h,
        &[
            "__record-op",
            "--resource",
            resource,
            "--output",
            &out,
            "--content",
            content,
        ],
    )
}

fn check(h: &Path) -> Output {
    run(h, &["state", "check"])
}

/// Every entry below `dir`: file bytes and mode, or the link target. Symlinks are not followed.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u32)> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, (Vec<u8>, u32)>) {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            let m = fs::symlink_metadata(&p).unwrap();
            // The lock file's note (pid and time) is rewritten by every writer that gets the
            // lock, including one that then refuses. It is not state.
            if p.ends_with("dotfiles-manager.lock") {
                continue;
            }
            if m.file_type().is_symlink() {
                let t = fs::read_link(&p).unwrap();
                out.insert(p, (t.to_str().unwrap().as_bytes().to_vec(), 0o120_000));
            } else if m.is_dir() {
                out.insert(p.clone(), (vec![], m.mode()));
                walk(&p, out);
            } else {
                // A FIFO or device has no bytes to compare, and reading a FIFO would block.
                let bytes = if m.is_file() {
                    fs::read(&p).unwrap()
                } else {
                    vec![]
                };
                out.insert(p.clone(), (bytes, m.mode()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, &mut out);
    out
}

fn names_below(dir: &Path) -> Vec<String> {
    snapshot(dir)
        .keys()
        .map(|p| p.strip_prefix(dir).unwrap().to_str().unwrap().to_string())
        .collect()
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).unwrap()
}

fn journal_phases(h: &Path) -> Vec<String> {
    fs::read_to_string(state(h).join("journal.jsonl"))
        .unwrap()
        .lines()
        .map(|l| json(l.as_bytes())["phase"].as_str().unwrap().to_string())
        .collect()
}

fn wait_for(what: &str, f: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn sha_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The ledger fingerprint of a regular file with this content.
fn fp_hex(s: &str) -> String {
    dfm_model::plan::file_fingerprint(s.as_bytes())
}

// ---- C01, C08: creation and layout --------------------------------------------------------------

#[test]
fn bind_creates_private_state_and_repeating_it_changes_nothing() {
    let h = bound_home("layout");
    let r = root(h.path());
    for (p, mode) in [
        (r.clone(), 0o700),
        (r.join("state"), 0o700),
        (r.join("state/receipts"), 0o700),
        (r.join("state/snapshots"), 0o700),
        (r.join("state/trials"), 0o700),
        (r.join("state/state.json"), 0o600),
        (r.join("state/ledger.json"), 0o600),
        (r.join("state/journal.jsonl"), 0o600),
        (h.path().join(".local/state/dotfiles-manager.lock"), 0o600),
        (
            h.path().join(".config/dotfiles-manager/machine.toml"),
            0o600,
        ),
    ] {
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o777, mode, "{p:?}");
    }
    let doc = fs::read_to_string(r.join("state/state.json")).unwrap();
    assert!(
        doc.contains(M5) && doc.contains("personal-dev-01.toml"),
        "{doc}"
    );
    let before = snapshot(h.path());
    let o = bind(h.path());
    assert_eq!(o.status.code(), Some(0));
    assert!(
        text(&o.stdout).trim().ends_with("already bound"),
        "{}",
        text(&o.stdout)
    );
    assert_eq!(
        snapshot(h.path()),
        before,
        "an idempotent bind writes nothing"
    );
    let c = check(h.path());
    assert_eq!(c.status.code(), Some(0), "{}", text(&c.stdout));
    assert!(text(&c.stdout).contains("state ok"), "{}", text(&c.stdout));
}

#[test]
fn state_check_before_any_bind_reports_unbound_and_writes_nothing() {
    let h = TempDir::new("unbound");
    let o = check(h.path());
    assert_eq!(o.status.code(), Some(0));
    assert!(text(&o.stdout).contains("unbound; no state"));
    assert!(names_below(h.path()).is_empty());
}

// ---- C02: intent first, independent completion ---------------------------------------------------

#[test]
fn an_operation_journals_intent_then_done_then_records_ownership_and_a_receipt() {
    let h = bound_home("lifecycle");
    let o = record(h.path(), "core.git", "gitconfig", "[user]\n");
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(journal_phases(h.path()), ["intent", "done"]);
    let intent = fs::read_to_string(state(h.path()).join("journal.jsonl")).unwrap();
    let first = intent.lines().next().unwrap();
    assert!(first.contains("\"existed_before\":false") && first.contains(&fp_hex("[user]\n")));

    let ledger = json(&fs::read(state(h.path()).join("ledger.json")).unwrap());
    let owned = &ledger["resources"][0];
    assert_eq!(owned["resource"], "core.git");
    assert_eq!(
        owned["owned_outputs"][0]["path"],
        out_file(h.path(), "gitconfig")
    );
    assert_eq!(owned["owned_outputs"][0]["fingerprint"], fp_hex("[user]\n"));
    let receipts = names_below(&state(h.path()).join("receipts"));
    assert_eq!(receipts.len(), 1, "{receipts:?}");
    let rp = state(h.path()).join("receipts").join(&receipts[0]);
    assert_eq!(fs::metadata(&rp).unwrap().mode() & 0o777, 0o600);
    let r = json(&fs::read(&rp).unwrap());
    assert_eq!(r["result"]["counts"]["updated"], 1);
    assert_eq!(r["result"]["exit_code"], 0);
    let c = check(h.path());
    assert_eq!(c.status.code(), Some(0), "{}", text(&c.stdout));
    assert!(text(&c.stdout).contains("1 resources, 1 owned outputs"));
    assert!(text(&c.stdout).contains("receipts: 1 (0 unreadable)"));
    // A second operation never claims a path that already exists.
    let again = record(h.path(), "core.other", "gitconfig", "x");
    assert_eq!(again.status.code(), Some(3), "{}", text(&again.stderr));
    assert!(
        text(&again.stderr).contains("E_STATE_INVALID"),
        "{}",
        text(&again.stderr)
    );
    assert_eq!(
        journal_phases(h.path()),
        ["intent", "done"],
        "refused before the intent"
    );
}

// ---- C05: abrupt exit, persistence failure, concurrency -------------------------------------------

#[test]
fn abrupt_exit_at_every_point_leaves_recoverable_state_and_a_free_lock() {
    for point in [
        "after-intent",
        "after-effect",
        "after-done",
        "ledger-after-tmp",
        "ledger-after-rename",
        "receipt-after-tmp",
        "receipt-after-rename",
    ] {
        let h = bound_home("abort");
        let first = record(h.path(), "core.base", "base", "base");
        assert_eq!(first.status.code(), Some(0));
        let good_ledger = fs::read(state(h.path()).join("ledger.json")).unwrap();

        let out = out_file(h.path(), "victim");
        let o = with_fail(
            h.path(),
            &format!("{point}:abort"),
            &[
                "__record-op",
                "--resource",
                "core.victim",
                "--output",
                &out,
                "--content",
                "v",
            ],
        );
        assert_eq!(o.status.signal(), Some(6), "{point}: {:?}", o.status);

        let c = check(h.path());
        let report = text(&c.stdout);
        let interrupted = ["after-intent", "after-effect"].contains(&point);
        // A crash between a temporary file and its rename leaves a stray entry: a finding.
        let stray = report.contains("stray entry");
        assert_eq!(
            c.status.code(),
            Some(i32::from(interrupted || stray)),
            "{point}: {report}"
        );
        assert_eq!(
            report.contains("E_OP_INTERRUPTED"),
            interrupted,
            "{point}: {report}"
        );
        assert!(
            !report
                .lines()
                .any(|l| l.contains("E_STATE_INVALID") && !l.contains("stray entry")),
            "{point}: {report}"
        );
        // The ledger is either the previous good one or the complete new one, never partial.
        let now = fs::read(state(h.path()).join("ledger.json")).unwrap();
        if point.starts_with("ledger-after-tmp") || interrupted || point == "after-done" {
            assert_eq!(now, good_ledger, "{point}: last good ledger preserved");
        }
        let owns = json(&now)["resources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["resource"] == "core.victim");
        let ledger_replaced = point == "ledger-after-rename" || point.starts_with("receipt");
        assert_eq!(
            owns, ledger_replaced,
            "{point}: ledger holds the victim only once replaced"
        );

        // Nothing grants ownership of the victim by inference.
        if interrupted || point == "after-done" {
            assert!(
                !text(&now).contains(&out),
                "{point}: ownership was inferred"
            );
        }
        // The lock was released by the kernel: a new writer runs, and reconcile settles the rest.
        let rec = run(h.path(), &["__reconcile"]);
        assert_eq!(rec.status.code(), Some(0), "{point}: {}", text(&rec.stderr));
        if interrupted {
            let settled = text(&rec.stdout);
            let want = if point == "after-intent" {
                "NotDone"
            } else {
                "Done"
            };
            let phase = if point == "after-intent" {
                "Failed"
            } else {
                "Done"
            };
            assert!(
                settled.contains(&format!("as {phase}")),
                "{point}: {settled} ({want})"
            );
            let after = fs::read(state(h.path()).join("ledger.json")).unwrap();
            assert_eq!(
                after, good_ledger,
                "{point}: reconcile never edits the ledger"
            );
            assert_eq!(check(h.path()).status.code(), Some(0), "{point}");
        }
    }
}

#[test]
fn a_persistence_failure_keeps_the_last_good_files_and_leaves_no_temporary_file() {
    for point in [
        "ledger-before-write",
        "ledger-after-tmp",
        "receipt-before-write",
        "receipt-after-tmp",
    ] {
        let h = bound_home("pfail");
        assert_eq!(
            record(h.path(), "core.base", "base", "b").status.code(),
            Some(0)
        );
        let before = snapshot(&state(h.path()));
        let out = out_file(h.path(), "next");
        let o = with_fail(
            h.path(),
            &format!("{point}:error"),
            &[
                "__record-op",
                "--resource",
                "core.next",
                "--output",
                &out,
                "--content",
                "n",
            ],
        );
        assert_eq!(o.status.code(), Some(1), "{point}: {}", text(&o.stderr));
        assert!(
            text(&o.stderr).contains("E_IO_FAILED"),
            "{}",
            text(&o.stderr)
        );
        let after = snapshot(&state(h.path()));
        let changed: Vec<_> = after
            .iter()
            .filter(|(k, v)| before.get(*k) != Some(*v))
            .map(|(k, _)| k.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        // The journal always moved on (intent and done were durable before the failure). At a
        // ledger point nothing else changed. At a receipt point the ledger had already been
        // replaced whole, and no receipt appeared.
        let allowed: &[&str] = if point.starts_with("ledger") {
            &["journal.jsonl"]
        } else {
            &["journal.jsonl", "ledger.json"]
        };
        assert!(
            changed.iter().all(|n| allowed.contains(&n.as_str())),
            "{point}: {changed:?}"
        );
        assert_eq!(
            names_below(&state(h.path()).join("receipts")).len(),
            1,
            "{point}"
        );
        assert!(
            names_below(&state(h.path()))
                .iter()
                .all(|n| !n.ends_with(".tmp")),
            "{point}"
        );
        // The manager lock is free again.
        assert_eq!(
            record(h.path(), "core.after", "after", "a").status.code(),
            Some(0),
            "{point}"
        );
    }
}

#[test]
fn a_second_writer_gets_exit_3_and_writes_nothing() {
    let h = bound_home("lock");
    let out = out_file(h.path(), "slow");
    // The holder waits at the failpoint until this file exists. No timing is involved.
    let release = h.path().join("release-holder");
    let mut slow = cmd(
        h.path(),
        &[
            "__record-op",
            "--resource",
            "core.slow",
            "--output",
            &out,
            "--content",
            "s",
        ],
    )
    .env(
        "DFM_FAILPOINT",
        format!("after-intent:wait-{}", release.display()),
    )
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
    let journal = state(h.path()).join("journal.jsonl");
    wait_for("the first intent", || {
        fs::metadata(&journal).is_ok_and(|m| m.len() > 0)
    });

    let before = snapshot(&state(h.path()));
    let second_out = out_file(h.path(), "second");
    let o = run(
        h.path(),
        &[
            "__record-op",
            "--resource",
            "core.second",
            "--output",
            &second_out,
            "--content",
            "2",
        ],
    );
    assert_eq!(o.status.code(), Some(3), "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("E_LOCKED"), "{}", text(&o.stderr));
    assert!(!Path::new(&second_out).exists());
    assert_eq!(
        snapshot(&state(h.path())),
        before,
        "the refused writer changed nothing"
    );

    fs::write(&release, "go").unwrap();
    assert_eq!(slow.wait().unwrap().code(), Some(0));
    assert_eq!(journal_phases(h.path()), ["intent", "done"]);
    assert_eq!(check(h.path()).status.code(), Some(0));
}

#[test]
fn state_check_while_a_writer_runs_reports_the_lock_and_no_findings() {
    let h = bound_home("check-locked");
    let out = out_file(h.path(), "slow");
    let release = h.path().join("release-holder");
    let mut slow = cmd(
        h.path(),
        &[
            "__record-op",
            "--resource",
            "core.slow",
            "--output",
            &out,
            "--content",
            "s",
        ],
    )
    .env(
        "DFM_FAILPOINT",
        format!("after-intent:wait-{}", release.display()),
    )
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
    let journal = state(h.path()).join("journal.jsonl");
    wait_for("the first intent", || {
        fs::metadata(&journal).is_ok_and(|m| m.len() > 0)
    });
    // A stray temp file is what a running apply leaves, and it must not be reported.
    fs::write(state(h.path()).join("receipts/.x.json.1.2.tmp"), "half").unwrap();

    let before = snapshot(&state(h.path()));
    let o = check(h.path());
    let (so, se) = (text(&o.stdout), text(&o.stderr));
    assert_eq!(o.status.code(), Some(3), "{so}{se}");
    assert!(se.contains("E_LOCKED"), "{se}");
    assert!(so.contains("writer is running"), "{so}");
    assert!(
        !so.contains("E_STATE_INVALID") && !so.contains("stray"),
        "{so}"
    );
    assert_eq!(snapshot(&state(h.path())), before, "check wrote nothing");

    fs::write(&release, "go").unwrap();
    assert_eq!(slow.wait().unwrap().code(), Some(0));
    fs::remove_file(state(h.path()).join("receipts/.x.json.1.2.tmp")).unwrap();
    assert_eq!(check(h.path()).status.code(), Some(0));
}

#[test]
fn state_check_holds_the_lock_across_its_whole_read() {
    let h = bound_home("check-holds");
    let release = h.path().join("release-check");
    let reached = h.path().join("release-check.reached");
    let mut c = cmd(h.path(), &["state", "check"])
        .env(
            "DFM_FAILPOINT",
            format!("check-after-probe:wait-{}", release.display()),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for("check to reach its read", || reached.exists());
    let out = out_file(h.path(), "late");
    let o = run(
        h.path(),
        &[
            "__record-op",
            "--resource",
            "core.late",
            "--output",
            &out,
            "--content",
            "l",
        ],
    );
    assert_eq!(o.status.code(), Some(3), "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("E_LOCKED"), "{}", text(&o.stderr));
    assert!(!Path::new(&out).exists());
    fs::write(&release, "go").unwrap();
    assert_eq!(c.wait().unwrap().code(), Some(0));
    // The lock is free again after check ends.
    assert_eq!(
        record(h.path(), "core.after", "after", "a").status.code(),
        Some(0)
    );
}

#[test]
fn state_check_refuses_when_the_lock_file_is_replaced_during_the_read() {
    let h = bound_home("check-lock-replaced");
    let release = h.path().join("release-replaced");
    let reached = h.path().join("release-replaced.reached");
    let c = cmd(h.path(), &["state", "check"])
        .env(
            "DFM_FAILPOINT",
            format!("check-after-probe:wait-{}", release.display()),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for("check to reach its read", || reached.exists());
    let lock = h.path().join(".local/state/dotfiles-manager.lock");
    assert!(lock.exists(), "the lock path moved; fix this test");
    let fresh = lock.with_extension("fresh");
    fs::write(&fresh, "").unwrap();
    fs::rename(&fresh, &lock).unwrap();
    fs::write(&release, "go").unwrap();
    let o = c.wait_with_output().unwrap();
    let (so, se) = (text(&o.stdout), text(&o.stderr));
    assert_eq!(o.status.code(), Some(3), "{so}{se}");
    assert!(
        se.contains("E_LOCKED") && se.contains("run the command again"),
        "{se}"
    );
    assert!(
        !so.contains("binding:") && !so.contains("E_STATE_INVALID"),
        "no findings: {so}"
    );
}

#[test]
fn state_check_refuses_when_a_first_bind_creates_the_lock_during_the_read() {
    // No lock file at the probe, so check holds no lock while it reads.
    let h = TempDir::new("check-lock-appears");
    let release = h.path().join("release-appears");
    let reached = h.path().join("release-appears.reached");
    let c = cmd(h.path(), &["state", "check"])
        .env(
            "DFM_FAILPOINT",
            format!("check-after-probe:wait-{}", release.display()),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for("check to reach its read", || reached.exists());
    let o = bind(h.path());
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    fs::write(&release, "go").unwrap();
    let o = c.wait_with_output().unwrap();
    let (so, se) = (text(&o.stdout), text(&o.stderr));
    assert_eq!(o.status.code(), Some(3), "{so}{se}");
    assert!(
        se.contains("E_LOCKED") && se.contains("run state check again"),
        "{se}"
    );
    assert!(
        !so.contains("binding:") && !so.contains("E_STATE_INVALID") && !so.contains("unbound"),
        "no findings: {so}"
    );
}

// ---- C06: identity against loaded receipts and trials --------------------------------------------

fn seeded(label: &str) -> TempDir {
    let h = bound_home(label);
    assert_eq!(
        record(h.path(), "core.git", "gitconfig", "g").status.code(),
        Some(0)
    );
    let base = out_file(h.path(), "tool-config");
    fs::write(&base, "baseline").unwrap();
    let o = run(
        h.path(),
        &[
            "__record-trial",
            "--id",
            "try-atuin",
            "--tool",
            "atuin",
            "--baseline-path",
            &base,
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    h
}

#[test]
fn a_trial_baseline_hash_is_the_file_fingerprint() {
    let h = seeded("baseline-fingerprint");
    let trial = text(&fs::read(state(h.path()).join("trials/try-atuin.json")).unwrap());
    let want = dfm_model::plan::file_fingerprint(b"baseline");
    assert!(trial.contains(&format!("\"sha256\":\"{want}\"")), "{trial}");
}

#[test]
fn receipts_and_trials_for_another_identity_are_reported_by_file() {
    let h = seeded("identity");
    let c = check(h.path());
    assert_eq!(c.status.code(), Some(0), "{}", text(&c.stdout));
    assert!(text(&c.stdout).contains("trials: 1 (0 unreadable)"));

    let dir = state(h.path());
    let receipt = dir
        .join("receipts")
        .join(&names_below(&dir.join("receipts"))[0]);
    let trial = dir.join("trials/try-atuin.json");
    let good = (fs::read(&receipt).unwrap(), fs::read(&trial).unwrap());
    for (path, original) in [(&receipt, &good.0), (&trial, &good.1)] {
        let swapped = text(original).replace(
            &format!("\"machine_id\":\"{M5}\""),
            "\"machine_id\":\"m3-work\"",
        );
        assert_ne!(swapped.as_bytes(), original.as_slice());
        fs::write(path, swapped).unwrap();
        let c = check(h.path());
        let report = text(&c.stdout);
        assert_eq!(c.status.code(), Some(1), "{report}");
        assert!(report.contains("E_IDENTITY_MISMATCH"), "{report}");
        assert!(
            report.contains(path.file_name().unwrap().to_str().unwrap()),
            "{report}"
        );
        assert!(text(&c.stderr).contains("E_DIAGNOSED"));
        fs::write(path, original).unwrap();
    }
    // An unreadable receipt is its own finding and hides nothing else.
    fs::write(
        dir.join("receipts/20260101T000000.000000000Z-broken.json"),
        "{",
    )
    .unwrap();
    fs::set_permissions(
        dir.join("receipts/20260101T000000.000000000Z-broken.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let report = text(&check(h.path()).stdout);
    assert!(
        report.contains("E_STATE_INVALID receipts/20260101T000000.000000000Z-broken.json"),
        "{report}"
    );
    assert!(
        report.contains("receipts: 2 (1 unreadable)")
            && report.contains("trials: 1 (0 unreadable)"),
        "{report}"
    );
    assert!(!report.contains("E_IDENTITY_MISMATCH"), "{report}");
}

#[test]
fn state_for_another_identity_cannot_be_used_even_with_a_matching_binding_file() {
    let h = seeded("foreign-state");
    let path = state(h.path()).join("state.json");
    let good = fs::read(&path).unwrap();
    fs::write(
        &path,
        text(&good).replace(
            &format!("\"machine_id\":\"{M5}\""),
            "\"machine_id\":\"m3-work\"",
        ),
    )
    .unwrap();
    let before = snapshot(h.path());
    let o = record(h.path(), "core.x", "x", "x");
    assert_eq!(o.status.code(), Some(3), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("E_IDENTITY_MISMATCH"),
        "{}",
        text(&o.stderr)
    );
    assert_eq!(snapshot(h.path()), before);
}

// ---- C07, C11: no second owner, no replacement ----------------------------------------------------

#[test]
fn a_changed_identity_is_refused_before_any_write_and_evidence_is_preserved() {
    let h = seeded("rebind");
    let before = snapshot(h.path());
    let (a, b) = (profile("personal-dev-01.toml"), profile("work-01.toml"));
    for args in [
        ["bind", "--profile", &b, "--machine-id", M5],
        ["bind", "--profile", &a, "--machine-id", "m3-work"],
        ["bind", "--profile", &b, "--machine-id", "m3-work"],
    ] {
        let o = run(h.path(), &args);
        assert_eq!(o.status.code(), Some(3), "{args:?}: {}", text(&o.stderr));
        assert!(
            text(&o.stderr).contains("E_REBIND_UNSUPPORTED"),
            "{}",
            text(&o.stderr)
        );
        assert_eq!(
            snapshot(h.path()),
            before,
            "{args:?}: nothing may change, not even timestamps' content"
        );
    }
    for args in [
        vec![
            "bind",
            "--force",
            "--profile",
            &b,
            "--machine-id",
            "m3-work",
        ],
        vec!["state", "check", "--profile", &b],
        vec![
            "__record-op",
            "--profile",
            &b,
            "--resource",
            "r",
            "--output",
            "/x",
        ],
    ] {
        assert_eq!(
            run(h.path(), &args).status.code(),
            Some(2),
            "{args:?}: there is no such flag"
        );
    }
    assert_eq!(snapshot(h.path()), before);
    assert_eq!(check(h.path()).status.code(), Some(0));
}

#[test]
fn state_left_by_a_crashed_bind_can_only_be_completed_by_the_same_identity() {
    let h = seeded("crashed-bind");
    let binding = h.path().join(".config/dotfiles-manager/machine.toml");
    fs::remove_file(&binding).unwrap();
    let before = snapshot(&root(h.path()));
    let o = run(
        h.path(),
        &[
            "bind",
            "--profile",
            &profile("work-01.toml"),
            "--machine-id",
            "m3-work",
        ],
    );
    assert_eq!(o.status.code(), Some(3), "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("E_REBIND_UNSUPPORTED"));
    assert!(!binding.exists());
    assert_eq!(snapshot(&root(h.path())), before);
    // The `state check` of a state with no binding is a finding, not an inferred profile.
    let c = check(h.path());
    assert_eq!(c.status.code(), Some(1));
    assert!(
        text(&c.stdout).contains("no binding"),
        "{}",
        text(&c.stdout)
    );
    // The same identity finishes the bind and leaves the existing state alone.
    assert_eq!(bind(h.path()).status.code(), Some(0));
    assert!(binding.exists());
    assert_eq!(snapshot(&root(h.path())), before);
    assert_eq!(check(h.path()).status.code(), Some(0));
}

#[test]
fn concurrent_initial_binds_create_one_owner_in_the_binding_and_in_the_state() {
    let (a, b) = (profile("personal-dev-01.toml"), profile("work-01.toml"));
    for round in 0..4 {
        let h = TempDir::new("race");
        let kids: Vec<_> = (0..8)
            .map(|i| {
                let p = if i % 2 == 0 { &a } else { &b };
                cmd(h.path(), &["bind", "--profile", p, "--machine-id", "m5"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let outs: Vec<_> = kids
            .into_iter()
            .map(|k| k.wait_with_output().unwrap())
            .collect();
        assert!(
            outs.iter().any(|o| o.status.code() == Some(0)),
            "round {round}: nobody bound"
        );
        for o in &outs {
            let err = text(&o.stderr);
            assert!(
                matches!(o.status.code(), Some(0))
                    || (matches!(o.status.code(), Some(3 | 1))
                        && (err.contains("E_REBIND_UNSUPPORTED") || err.contains("E_LOCKED"))),
                "round {round}: {:?} {err}",
                o.status
            );
        }
        let binding =
            fs::read_to_string(h.path().join(".config/dotfiles-manager/machine.toml")).unwrap();
        let st = fs::read_to_string(state(h.path()).join("state.json")).unwrap();
        let ledger = fs::read_to_string(state(h.path()).join("ledger.json")).unwrap();
        let winner = if binding.contains("personal-dev-01.toml") {
            "personal-dev-01.toml"
        } else {
            "work-01.toml"
        };
        let loser = if winner == "work-01.toml" {
            "personal-dev-01.toml"
        } else {
            "work-01.toml"
        };
        assert!(
            st.contains(winner) && ledger.contains(winner),
            "round {round}"
        );
        assert!(
            !st.contains(loser) && !ledger.contains(loser),
            "round {round}: two owners"
        );
        assert_eq!(check(h.path()).status.code(), Some(0), "round {round}");
        let stray: Vec<_> = names_below(&root(h.path()))
            .into_iter()
            .filter(|n| n.contains(".tmp") || n.contains("state.next"))
            .collect();
        assert!(stray.is_empty(), "round {round}: {stray:?}");
    }
}

#[test]
fn state_check_counts_stray_entries_as_findings() {
    let h = bound_home("stray-findings");
    assert_eq!(check(h.path()).status.code(), Some(0));
    for f in [
        root(h.path()).join("state.next"),
        state(h.path()).join("receipts/.20261010T000000.000000000Z-run.json.1.2.tmp"),
        state(h.path()).join("trials/.t.json.1.2.tmp"),
    ] {
        fs::write(&f, "half").unwrap();
        let o = check(h.path());
        let out = text(&o.stdout);
        assert_eq!(o.status.code(), Some(1), "{f:?}: {out}");
        assert!(out.contains("E_STATE_INVALID"), "{out}");
        assert!(!out.contains("state ok"), "{out}");
        fs::remove_file(&f).unwrap();
    }
    assert_eq!(check(h.path()).status.code(), Some(0));
}

// ---- C08: topology ---------------------------------------------------------------------------------

#[test]
fn redirected_manager_paths_are_refused_with_zero_writes_to_the_destination() {
    let foreign = TempDir::new("foreign");
    let cases: [(&str, &[&str]); 6] = [
        (".local/state/dotfiles-manager", &["bind"]),
        (".config/dotfiles-manager", &["bind"]),
        (".local", &["bind"]),
        (".local/state/dotfiles-manager.lock", &["bind"]),
        (".local/state/dotfiles-manager/state", &["state", "check"]),
        (
            ".local/state/dotfiles-manager/state/receipts",
            &["state", "check"],
        ),
    ];
    for (victim, then) in cases {
        let h = if then == ["state", "check"] {
            bound_home("redirect")
        } else {
            TempDir::new("redirect")
        };
        let link = h.path().join(victim);
        if then == ["bind"] {
            fs::create_dir_all(link.parent().unwrap()).unwrap();
        } else {
            fs::remove_dir_all(&link).unwrap();
        }
        symlink(foreign.path(), &link).unwrap();
        let pr = profile("personal-dev-01.toml");
        let args: Vec<&str> = if then == ["bind"] {
            vec!["bind", "--profile", &pr, "--machine-id", M5]
        } else {
            then.to_vec()
        };
        let o = run(h.path(), &args);
        let err = text(&o.stderr);
        // bind refuses before any write (3). The read-only check reports a finding (1).
        let want = if then == ["bind"] { 3 } else { 1 };
        assert_eq!(o.status.code(), Some(want), "{victim}: {err}");
        let said = format!("{err}{}", text(&o.stdout));
        assert!(
            said.contains("E_STORAGE_INVALID") || said.contains("E_BINDING_INVALID"),
            "{victim}: {said}"
        );
        assert!(
            names_below(foreign.path()).is_empty(),
            "{victim}: the foreign directory was written"
        );
    }
    // The same for the writer paths of an already bound machine.
    let h = bound_home("redirect-writer");
    fs::remove_dir_all(state(h.path()).join("trials")).unwrap();
    symlink(foreign.path(), state(h.path()).join("trials")).unwrap();
    let base = out_file(h.path(), "t");
    fs::write(&base, "b").unwrap();
    let o = run(
        h.path(),
        &[
            "__record-trial",
            "--id",
            "t1",
            "--tool",
            "x",
            "--baseline-path",
            &base,
        ],
    );
    assert_eq!(o.status.code(), Some(3), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("E_STORAGE_INVALID"),
        "{}",
        text(&o.stderr)
    );
    assert!(names_below(foreign.path()).is_empty());
}

#[test]
fn conflicting_path_types_and_loose_permissions_are_refused_not_repaired() {
    let h = TempDir::new("types");
    let r = root(h.path());
    fs::create_dir_all(r.parent().unwrap()).unwrap();
    fs::write(&r, "i am a file").unwrap();
    let before = snapshot(h.path());
    let o = bind(h.path());
    assert_eq!(o.status.code(), Some(3), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("E_STORAGE_INVALID"),
        "{}",
        text(&o.stderr)
    );
    assert_eq!(snapshot(h.path()), before.clone());

    for (label, path, mode) in [
        ("root", ".local/state/dotfiles-manager", 0o755),
        ("state", ".local/state/dotfiles-manager/state", 0o750),
        (
            "receipts",
            ".local/state/dotfiles-manager/state/receipts",
            0o777,
        ),
    ] {
        let h = bound_home("modes");
        let p = h.path().join(path);
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
        let o = check(h.path());
        assert_eq!(o.status.code(), Some(1), "{label}: {}", text(&o.stderr));
        assert!(text(&o.stdout).contains("E_STORAGE_INVALID"), "{label}");
        let o = record(h.path(), "core.x", "x", "x");
        assert_eq!(o.status.code(), Some(3), "{label}");
        assert!(
            text(&o.stderr).contains("E_STORAGE_INVALID"),
            "{}",
            text(&o.stderr)
        );
        assert!(
            !h.path().join("x").exists(),
            "{label}: refused before the effect"
        );
        assert_eq!(
            fs::metadata(&p).unwrap().mode() & 0o777,
            mode,
            "{label}: not repaired"
        );
    }
    // A loose file mode is refused too.
    let h = bound_home("filemode");
    let l = state(h.path()).join("ledger.json");
    fs::set_permissions(&l, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(check(h.path()).status.code(), Some(1));
    assert!(text(&check(h.path()).stdout).contains("E_STORAGE_INVALID ledger.json"));
}

#[test]
fn a_hard_linked_state_document_is_a_finding() {
    let h = bound_home("hardlink");
    let j = state(h.path()).join("journal.jsonl");
    fs::hard_link(&j, h.path().join("second-name")).unwrap();
    let c = check(h.path());
    assert_eq!(c.status.code(), Some(1), "{}", text(&c.stdout));
    assert!(
        text(&c.stdout).contains("E_STORAGE_INVALID journal.jsonl"),
        "{}",
        text(&c.stdout)
    );
}

#[test]
fn a_legitimate_alias_of_home_is_recognized() {
    // macOS reaches temporary directories through /var, which is a link to /private/var.
    let h = TempDir::new("alias");
    let canonical = h.path().to_str().unwrap();
    if let Some(alias) = canonical.strip_prefix("/private") {
        assert_eq!(fs::canonicalize(alias).unwrap(), h.path());
        let o = bind(Path::new(alias));
        assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
        assert_eq!(check(Path::new(alias)).status.code(), Some(0));
        assert_eq!(
            check(h.path()).status.code(),
            Some(0),
            "same state through both names"
        );
    }
    // A link above HOME is an alias too.
    let real = TempDir::new("alias-real");
    let holder = TempDir::new("alias-holder");
    let link = holder.path().join("home");
    symlink(real.path(), &link).unwrap();
    let o = bind(&link);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert!(
        real.path()
            .join(".local/state/dotfiles-manager/state/state.json")
            .exists()
    );
    assert_eq!(check(real.path()).status.code(), Some(0));
}

// ---- C03: missing, corrupt, unsupported ---------------------------------------------------------------

#[test]
fn missing_corrupt_or_unsupported_state_grants_no_authority_and_is_not_rewritten() {
    let corrupt: [(&str, Option<&str>, &str); 4] = [
        ("missing", None, "E_STATE_INVALID"),
        ("garbage", Some("not json"), "E_STATE_INVALID"),
        ("newer", Some("{\"schema\":2}"), "E_STATE_NEWER"),
        ("old", Some("{\"schema\":0}"), "E_SCHEMA_UNSUPPORTED"),
    ];
    for (label, content, code) in corrupt {
        let h = seeded(label);
        let ledger = state(h.path()).join("ledger.json");
        match content {
            None => fs::remove_file(&ledger).unwrap(),
            Some(c) => fs::write(&ledger, c).unwrap(),
        }
        let before = snapshot(h.path());
        let c = check(h.path());
        let report = text(&c.stdout);
        assert_eq!(c.status.code(), Some(1), "{label}: {report}");
        assert!(
            report.contains(&format!("{code} ledger.json")),
            "{label}: {report}"
        );
        assert!(
            !report.contains("owned outputs"),
            "{label}: the report names no owned output"
        );
        let o = record(h.path(), "core.y", "y", "y");
        assert_eq!(o.status.code(), Some(3), "{label}: {}", text(&o.stderr));
        assert!(
            text(&o.stderr).contains(code),
            "{label}: {}",
            text(&o.stderr)
        );
        assert!(!h.path().join("y").exists());
        assert_eq!(
            snapshot(h.path()),
            before,
            "{label}: nothing was repaired or rewritten"
        );
    }
    // Lost state.json with the rest present: bind will not rebuild it, even for the same identity.
    let h = seeded("lost");
    fs::remove_file(state(h.path()).join("state.json")).unwrap();
    fs::remove_file(h.path().join(".config/dotfiles-manager/machine.toml")).unwrap();
    let before = snapshot(h.path());
    let o = bind(h.path());
    assert_eq!(o.status.code(), Some(3), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("E_STATE_INVALID"),
        "{}",
        text(&o.stderr)
    );
    assert_eq!(snapshot(h.path()), before);
    // A newer state.json is refused by bind with its own code.
    let h = bound_home("newer-state");
    fs::write(state(h.path()).join("state.json"), "{\"schema\":9}").unwrap();
    let o = bind(h.path());
    assert_eq!(o.status.code(), Some(3));
    assert!(
        text(&o.stderr).contains("E_STATE_NEWER"),
        "{}",
        text(&o.stderr)
    );
}

// ---- C04: secrets and scope ---------------------------------------------------------------------------

#[test]
fn a_planted_secret_and_its_hash_never_reach_state_or_output() {
    let h = bound_home("secret");
    let secret = "hunter2-planted-secret";
    let out = out_file(h.path(), ".token");
    let o = run(
        h.path(),
        &[
            "__record-op",
            "--resource",
            "personal.token",
            "--adapter",
            "op-template/v1",
            "--output",
            &out,
            "--content",
            secret,
            "--sensitive",
            "yes",
            "--note",
            &format!("provider said: auth failed for {secret}\x1b[31m"),
            "--secret",
            secret,
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let (digest, plain) = (fp_hex(secret), secret.to_string());
    let raw = sha_hex(secret);
    let mut seen = 0;
    let mut stack = vec![root(h.path()), h.path().join(".config")];
    while let Some(p) = stack.pop() {
        for e in fs::read_dir(&p).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
                continue;
            }
            // Bytes, not text: a non-UTF-8 file must be scanned too, and a read error must fail.
            let body = fs::read(e.path()).unwrap_or_else(|err| panic!("{:?}: {err}", e.path()));
            let has = |needle: &str| body.windows(needle.len()).any(|w| w == needle.as_bytes());
            assert!(!has(&plain), "{:?} holds the secret", e.path());
            assert!(
                !has(&digest) && !has(&raw),
                "{:?} holds the secret's hash",
                e.path()
            );
            seen += 1;
        }
    }
    assert!(seen >= 5, "walked {seen} files");
    for stream in [
        &o.stdout,
        &o.stderr,
        &check(h.path()).stdout,
        &check(h.path()).stderr,
    ] {
        let s = text(stream);
        assert!(
            !s.contains(&plain) && !s.contains(&digest) && !s.contains(&raw),
            "{s}"
        );
    }
    let ledger = fs::read_to_string(state(h.path()).join("ledger.json")).unwrap();
    assert!(
        ledger.contains("\"class\":\"sensitive\"") && ledger.contains("\"fingerprint\":null"),
        "{ledger}"
    );
    let receipts = names_below(&state(h.path()).join("receipts"));
    let r = fs::read_to_string(state(h.path()).join("receipts").join(&receipts[0])).unwrap();
    assert!(
        r.contains("[redacted]") && r.contains("\"after\":null") && r.contains("\"before\":null"),
        "{r}"
    );
    assert!(
        r.contains("\\u{1b}"),
        "control characters are visible escapes: {r}"
    );
    assert_eq!(
        fs::read_to_string(&out).unwrap(),
        secret,
        "the application-owned output itself is untouched"
    );
    assert!(
        names_below(&state(h.path()).join("snapshots")).is_empty(),
        "no snapshot content for a sensitive resource"
    );
}

#[test]
fn claims_are_limited_to_exact_paths_inside_home() {
    let h = bound_home("scope");
    let before = snapshot(h.path());
    let manager_files = [
        state(h.path())
            .join("ledger.json")
            .to_str()
            .unwrap()
            .to_string(),
        h.path()
            .join(".config/dotfiles-manager/machine.toml")
            .to_str()
            .unwrap()
            .to_string(),
        h.path().to_str().unwrap().to_string(),
        "/etc/dotfiles-manager-test".to_string(),
    ];
    for path in manager_files {
        let o = run(
            h.path(),
            &[
                "__record-op",
                "--resource",
                "core.bad",
                "--output",
                &path,
                "--content",
                "x",
            ],
        );
        assert_eq!(o.status.code(), Some(3), "{path}: {}", text(&o.stderr));
        assert!(
            text(&o.stderr).contains("E_STATE_INVALID"),
            "{path}: {}",
            text(&o.stderr)
        );
        assert_eq!(snapshot(h.path()), before, "{path}");
    }
    assert!(!Path::new("/etc/dotfiles-manager-test").exists());
}

// ---- review round: exit codes, names, tampering ---------------------------------------------------

#[test]
fn a_refusal_after_the_effect_exits_1_not_3() {
    let h = bound_home("mutated");
    let run_with = |name: &str| {
        let out = out_file(h.path(), name);
        run(
            h.path(),
            &[
                "__record-op",
                "--resource",
                "core.dup",
                "--output",
                &out,
                "--content",
                "x",
                "--op-id",
                "dup",
                "--at",
                "1791590400000000000",
            ],
        )
    };
    assert_eq!(run_with("first").status.code(), Some(0));
    // Same run id and start time: the receipt name collides after the effect already happened.
    let o = run_with("second");
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("E_STORAGE_INVALID"),
        "{}",
        text(&o.stderr)
    );
    assert!(h.path().join("second").exists(), "the effect did happen");
    // A refusal before the intent is still a gate: exit 3.
    let o = record(h.path(), "core.dup", "first", "x");
    assert_eq!(o.status.code(), Some(3));
    assert!(
        text(&o.stderr).contains("E_STATE_INVALID"),
        "{}",
        text(&o.stderr)
    );
}

#[test]
fn a_secret_in_a_file_name_is_refused_before_anything_is_written() {
    let h = bound_home("secret-name");
    let before = snapshot(h.path());
    let named = out_file(h.path(), "tok-hunter2");
    let o = run(
        h.path(),
        &[
            "__record-op",
            "--resource",
            "personal.token",
            "--output",
            &named,
            "--content",
            "c",
            "--secret",
            "hunter2",
        ],
    );
    assert_eq!(o.status.code(), Some(3), "{}", text(&o.stderr));
    let said = format!("{}{}", text(&o.stdout), text(&o.stderr));
    assert!(!said.contains("hunter2"), "{said}");
    assert!(!Path::new(&named).exists());
    assert_eq!(snapshot(h.path()), before);
}

#[test]
fn a_hand_written_ledger_cannot_grant_authority_over_foreign_paths() {
    let h = seeded("handwritten");
    let path = state(h.path()).join("ledger.json");
    for target in [
        "/etc/hosts".to_string(),
        state(h.path())
            .join("state.json")
            .to_str()
            .unwrap()
            .to_string(),
    ] {
        let mut l = json(&fs::read(&path).unwrap());
        l["resources"][0]["owned_outputs"][0]["path"] = target.clone().into();
        fs::write(&path, serde_json::to_vec(&l).unwrap()).unwrap();
        let c = check(h.path());
        let report = text(&c.stdout);
        assert_eq!(c.status.code(), Some(1), "{target}: {report}");
        assert!(
            report.contains("E_STATE_INVALID ledger.json"),
            "{target}: {report}"
        );
        assert!(!report.contains("owned outputs"), "{target}: {report}");
        let o = record(h.path(), "core.z", "z", "z");
        assert_eq!(o.status.code(), Some(3), "{target}");
        assert!(
            text(&o.stderr).contains("E_STATE_INVALID"),
            "{}",
            text(&o.stderr)
        );
        assert!(!h.path().join("z").exists());
    }
}

#[test]
fn a_ledger_for_another_machine_stops_the_writer_before_any_effect() {
    let h = seeded("foreign-ledger");
    let path = state(h.path()).join("ledger.json");
    let good = fs::read(&path).unwrap();
    let swapped = text(&good).replace(
        &format!("\"machine_id\":\"{M5}\""),
        "\"machine_id\":\"m3-work\"",
    );
    fs::write(&path, &swapped).unwrap();
    let o = record(h.path(), "core.z", "z", "z");
    assert_eq!(o.status.code(), Some(3), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("E_IDENTITY_MISMATCH"),
        "{}",
        text(&o.stderr)
    );
    assert!(!h.path().join("z").exists());
    assert_eq!(fs::read_to_string(&path).unwrap(), swapped, "left alone");
}

#[test]
fn a_holder_whose_lock_file_was_removed_stops_writing() {
    let h = bound_home("lock-removed");
    let out = out_file(h.path(), "held");
    let release = h.path().join("release-holder");
    let holder = cmd(
        h.path(),
        &[
            "__record-op",
            "--resource",
            "core.held",
            "--output",
            &out,
            "--content",
            "h",
        ],
    )
    .env(
        "DFM_FAILPOINT",
        format!("after-effect:wait-{}", release.display()),
    )
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    wait_for("the effect", || Path::new(&out).exists());
    fs::remove_file(h.path().join(".local/state/dotfiles-manager.lock")).unwrap();
    fs::write(&release, "go").unwrap();
    let o = holder.wait_with_output().unwrap();
    // The effect happened, then the next write was refused: after a mutation, so exit 1.
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("E_LOCKED"), "{}", text(&o.stderr));
    assert_eq!(
        journal_phases(h.path()),
        ["intent"],
        "no outcome was written without the lock"
    );
    let ledger = json(&fs::read(state(h.path()).join("ledger.json")).unwrap());
    assert!(ledger["resources"].as_array().unwrap().is_empty());
}

#[test]
fn a_torn_journal_is_reported_and_reconcile_leaves_it_alone() {
    let h = bound_home("torn");
    let out = out_file(h.path(), "victim");
    let o = with_fail(
        h.path(),
        "after-effect:abort",
        &[
            "__record-op",
            "--resource",
            "core.victim",
            "--output",
            &out,
            "--content",
            "v",
        ],
    );
    assert_eq!(o.status.signal(), Some(6));
    let j = state(h.path()).join("journal.jsonl");
    let mut bytes = fs::read(&j).unwrap();
    bytes.truncate(bytes.len() - 5);
    fs::write(&j, &bytes).unwrap();
    let c = check(h.path());
    assert_eq!(c.status.code(), Some(1));
    assert!(
        text(&c.stdout).contains("E_STATE_INVALID journal.jsonl"),
        "{}",
        text(&c.stdout)
    );
    let r = run(h.path(), &["__reconcile"]);
    assert_eq!(r.status.code(), Some(3), "{}", text(&r.stderr));
    assert!(
        text(&r.stderr).contains("E_STATE_INVALID"),
        "{}",
        text(&r.stderr)
    );
    assert_eq!(fs::read(&j).unwrap(), bytes);
}

#[test]
fn recovery_counts_staged_copy_removals_before_a_journal_refusal() {
    use dfm_model::state::{
        Class, JournalEntry, MAX_JOURNAL_BYTES, Phase, Recovery, RecoveryOutput,
    };

    let h = bound_home("recovery-sweep-refusal");
    let intent = JournalEntry {
        schema: 1,
        op_id: "sweep-1".into(),
        op: "create-file".into(),
        phase: Phase::Intent,
        at: "2026-10-10T00:00:00.000000000Z".into(),
        resource: "core.x".into(),
        adapter: "git-config/v1".into(),
        class: Class::Normal,
        recovery: Some(Recovery {
            outputs: vec![RecoveryOutput {
                path: out_file(h.path(), "file"),
                existed_before: false,
                expected_sha256: Some(fp_hex("staged")),
            }],
            snapshot: None,
        }),
        token: None,
        reason: None,
    };
    let tail = intent.to_line().unwrap();
    let mut done = JournalEntry {
        op_id: "closed-1".into(),
        phase: Phase::Done,
        recovery: None,
        reason: Some("x".repeat(1024)),
        ..intent.clone()
    };
    let line = done.to_line().unwrap();
    let room = MAX_JOURNAL_BYTES as usize - tail.len();
    let mut journal = line.repeat(room / line.len() - 1);
    done.reason = Some("x".repeat(1024 + room % line.len()));
    journal.extend(done.to_line().unwrap());
    journal.extend(tail);
    assert_eq!(journal.len(), MAX_JOURNAL_BYTES as usize);
    let path = state(h.path()).join("journal.jsonl");
    fs::write(&path, &journal).unwrap();

    let tag = sha_hex(&format!("{}\0{}", intent.op_id, intent.at));
    let tmp = h.path().join(format!(".file.{}.tmp", &tag[..16]));
    let stage = tmp.with_extension("stage");
    for (entry, exit) in [(None, 3), (Some(&tmp), 1), (Some(&stage), 1)] {
        if let Some(entry) = entry {
            if entry == &stage {
                symlink("file", entry).unwrap();
            } else {
                fs::write(entry, "staged").unwrap();
            }
        }
        let r = run(h.path(), &["state", "recover"]);
        assert_eq!(r.status.code(), Some(exit), "{}", text(&r.stderr));
        assert!(text(&r.stderr).contains("E_LIMIT_EXCEEDED"));
        assert!(fs::symlink_metadata(&tmp).is_err());
        assert!(fs::symlink_metadata(&stage).is_err());
        assert_eq!(fs::read(&path).unwrap(), journal);
    }
    // A staged name that cannot be removed does not count as a mutation.
    fs::create_dir(&tmp).unwrap();
    let r = run(h.path(), &["state", "recover"]);
    assert_eq!(r.status.code(), Some(3), "{}", text(&r.stderr));
    assert!(tmp.is_dir());
}

#[test]
fn recover_repairs_one_torn_final_journal_line_and_refuses_a_torn_middle_line() {
    let h = bound_home("torn-repair");
    let o = record(h.path(), "core.victim", "victim", "v");
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let j = state(h.path()).join("journal.jsonl");
    let sound = fs::read(&j).unwrap();
    let mut torn = sound.clone();
    torn.extend_from_slice(b"{\"schema\":1,\"op_");
    fs::write(&j, &torn).unwrap();
    assert_ne!(check(h.path()).status.code(), Some(0));
    let r = run(h.path(), &["state", "recover"]);
    assert_eq!(r.status.code(), Some(0), "{}", text(&r.stderr));
    assert!(
        text(&r.stdout).contains("repaired a torn final journal line (16 bytes removed)"),
        "{}",
        text(&r.stdout)
    );
    let now = fs::read(&j).unwrap();
    assert!(now.starts_with(&sound));
    assert!(text(&now).contains("torn line repaired"));
    assert_eq!(check(h.path()).status.code(), Some(0));
    // The next write works.
    let o = record(h.path(), "core.second", "second", "v");
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    // A damaged line that is not the last one stays a hard error and is not touched.
    let mut bad = fs::read(&j).unwrap();
    bad[3] = b'#';
    bad.extend_from_slice(b"{\"torn");
    fs::write(&j, &bad).unwrap();
    let r = run(h.path(), &["state", "recover"]);
    assert_eq!(r.status.code(), Some(3), "{}", text(&r.stderr));
    assert!(
        text(&r.stderr).contains("E_STATE_INVALID"),
        "{}",
        text(&r.stderr)
    );
    assert_eq!(fs::read(&j).unwrap(), bad);
}

#[test]
fn an_abort_after_the_cut_leaves_the_cut_text_and_the_next_run_reports_it() {
    let h = bound_home("torn-abort");
    assert_eq!(
        record(h.path(), "core.victim", "victim", "v").status.code(),
        Some(0)
    );
    let j = state(h.path()).join("journal.jsonl");
    let mut torn = fs::read(&j).unwrap();
    let sound = torn.len();
    torn.extend_from_slice(b"{\"schema\":1,\"op_");
    fs::write(&j, &torn).unwrap();
    // The process dies after the cut, before the repair record is written.
    let r = with_fail(
        h.path(),
        "journal-before-write:abort",
        &["state", "recover"],
    );
    assert_ne!(r.status.code(), Some(0));
    assert_eq!(fs::read(&j).unwrap().len(), sound, "the torn line was cut");
    let kept = state(h.path()).join("journal.torn");
    assert_eq!(fs::read(&kept).unwrap(), b"{\"schema\":1,\"op_");

    // The next run finds nothing torn. It must not lose the cut text, and the check must say so.
    let r = run(h.path(), &["state", "recover"]);
    assert_eq!(r.status.code(), Some(0), "{}", text(&r.stderr));
    assert_eq!(fs::read(&kept).unwrap(), b"{\"schema\":1,\"op_");
    let c = check(h.path());
    assert_eq!(c.status.code(), Some(1), "{}", text(&c.stdout));
    assert!(
        text(&c.stdout).contains("journal.torn"),
        "{}",
        text(&c.stdout)
    );
}

#[test]
fn a_failed_repair_after_the_cut_exits_1_because_the_journal_changed() {
    let h = bound_home("torn-cut");
    assert_eq!(
        record(h.path(), "core.victim", "victim", "v").status.code(),
        Some(0)
    );
    let j = state(h.path()).join("journal.jsonl");
    let mut torn = fs::read(&j).unwrap();
    let sound = torn.len();
    torn.extend_from_slice(b"{\"schema\":1,\"op_");
    fs::write(&j, &torn).unwrap();
    let r = with_fail(
        h.path(),
        "journal-before-write:error",
        &["state", "recover"],
    );
    assert_eq!(r.status.code(), Some(1), "{}", text(&r.stderr));
    assert_eq!(fs::read(&j).unwrap().len(), sound, "the torn line was cut");
}

#[test]
fn an_unreadable_receipts_directory_is_a_finding_not_an_empty_list() {
    let h = seeded("unreadable");
    let dir = state(h.path()).join("receipts");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
    let c = check(h.path());
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(c.status.code(), Some(1), "{}", text(&c.stderr));
    assert!(
        text(&c.stdout).contains("E_STORAGE_INVALID"),
        "{}",
        text(&c.stdout)
    );
    assert!(!text(&c.stdout).contains("state ok"));
}

// ---- final round: state loss -------------------------------------------------------------------

#[test]
fn a_binding_without_its_state_is_state_loss_and_is_never_recreated() {
    for lose in ["root", "state-dir"] {
        let h = seeded("loss");
        let gone = if lose == "root" {
            root(h.path())
        } else {
            state(h.path())
        };
        fs::remove_dir_all(&gone).unwrap();
        let before = snapshot(h.path());
        let o = bind(h.path());
        assert_eq!(o.status.code(), Some(3), "{lose}: {}", text(&o.stderr));
        assert!(
            text(&o.stderr).contains("E_STATE_INVALID"),
            "{lose}: {}",
            text(&o.stderr)
        );
        assert!(
            text(&o.stderr).contains("state loss"),
            "{lose}: {}",
            text(&o.stderr)
        );
        assert_eq!(snapshot(h.path()), before, "{lose}: nothing was written");
        let c = check(h.path());
        assert_eq!(c.status.code(), Some(1), "{lose}: {}", text(&c.stdout));
        assert!(
            text(&c.stdout).contains("E_STATE_INVALID"),
            "{lose}: {}",
            text(&c.stdout)
        );
        assert!(text(&c.stdout).contains("state loss"), "{lose}");
        // Writers refuse too, before any effect.
        let o = record(h.path(), "core.y", "y", "y");
        assert_eq!(o.status.code(), Some(3), "{lose}");
        assert!(
            text(&o.stderr).contains("E_STATE_INVALID"),
            "{}",
            text(&o.stderr)
        );
        assert!(!h.path().join("y").exists());
        assert_eq!(snapshot(h.path()), before, "{lose}");
    }
}

// ---- DFM-57 WP-B ---------------------------------------------------------------------------------

#[test]
fn a_writer_refuses_a_state_store_that_lacks_a_subdirectory_before_any_change() {
    for missing in ["receipts", "snapshots", "trials"] {
        let h = bound_home("missing-sub");
        fs::remove_dir(state(h.path()).join(missing)).unwrap();
        let before = snapshot(h.path());
        // Each command that opens the store for writing refuses, and nothing changes.
        let o = record(h.path(), "core.y", "y", "y");
        assert_eq!(o.status.code(), Some(3), "{missing}: {}", text(&o.stderr));
        assert!(
            text(&o.stderr).contains("E_STORAGE_INVALID"),
            "{missing}: {}",
            text(&o.stderr)
        );
        assert!(
            text(&o.stderr).contains(missing),
            "{missing}: {}",
            text(&o.stderr)
        );
        let o = run(h.path(), &["state", "recover"]);
        assert_eq!(o.status.code(), Some(3), "{missing}: {}", text(&o.stderr));
        assert!(!h.path().join("y").exists(), "{missing}");
        assert_eq!(
            snapshot(h.path()),
            before,
            "{missing}: HOME and the store are unchanged"
        );
        // `state check` still reports the same layout as a finding.
        let c = check(h.path());
        assert_eq!(c.status.code(), Some(1), "{missing}: {}", text(&c.stdout));
        assert!(
            text(&c.stdout).contains("this directory is missing"),
            "{missing}"
        );
    }
}

#[test]
fn resolve_and_a_first_bind_refuse_a_state_store_that_lacks_a_subdirectory_before_any_change() {
    for missing in ["receipts", "snapshots", "trials"] {
        let h = bound_home("missing-sub-cmds");
        fs::remove_dir(state(h.path()).join(missing)).unwrap();
        let before = snapshot(h.path());
        let resolve = run(h.path(), &["state", "resolve", "nope", "--as", "failed"]);
        assert_eq!(resolve.status.code(), Some(3), "{}", text(&resolve.stderr));
        assert!(
            text(&resolve.stderr).contains("E_STORAGE_INVALID"),
            "{missing}"
        );
        assert_eq!(
            snapshot(h.path()),
            before,
            "{missing}: resolve changed nothing"
        );

        // Without its binding file, a bind writes: it takes the same gate as the other writers.
        fs::remove_dir_all(h.path().join(".config/dotfiles-manager")).unwrap();
        let before = snapshot(h.path());
        let o = bind(h.path());
        assert_eq!(o.status.code(), Some(3), "{missing}: {}", text(&o.stderr));
        assert!(text(&o.stderr).contains("E_STORAGE_INVALID"), "{missing}");
        assert_eq!(
            snapshot(h.path()),
            before,
            "{missing}: bind changed nothing"
        );
    }
}

#[test]
fn a_first_bind_refuses_an_empty_state_directory_before_any_write() {
    // The engine's `init` accepts an empty `state/`, but `bind` reads the store first (phase 1)
    // and a store without `state.json` is state loss, so nothing reaches `init`.
    let h = bound_home("empty-state");
    fs::remove_dir_all(h.path().join(".config/dotfiles-manager")).unwrap();
    let st = state(h.path());
    fs::remove_dir_all(&st).unwrap();
    fs::create_dir(&st).unwrap();
    fs::set_permissions(&st, fs::Permissions::from_mode(0o700)).unwrap();
    let before = snapshot(h.path());
    let o = bind(h.path());
    assert_eq!(o.status.code(), Some(3), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("E_STATE_INVALID"),
        "{}",
        text(&o.stderr)
    );
    assert_eq!(snapshot(h.path()), before);
}

fn record_trial(h: &Path, id: &str, baseline: &Path) -> Output {
    run(
        h,
        &[
            "__record-trial",
            "--id",
            id,
            "--tool",
            "atuin",
            "--baseline-path",
            baseline.to_str().unwrap(),
        ],
    )
}

#[test]
fn a_trial_baseline_that_cannot_be_read_or_is_a_link_is_an_error_not_an_absent_file() {
    let h = bound_home("baseline");
    let trial = state(h.path()).join("trials/t1.json");
    // A missing path is recorded as absent; a plain file is recorded with its file fingerprint.
    let o = record_trial(h.path(), "t1", &h.path().join("nothing"));
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(
        json(&fs::read(&trial).unwrap())["baseline"]["paths"][0]["existed"],
        false
    );
    let plain = h.path().join("plain");
    fs::write(&plain, "baseline").unwrap();
    let o = record_trial(h.path(), "t2", &plain);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let t2 = json(&fs::read(state(h.path()).join("trials/t2.json")).unwrap());
    assert_eq!(t2["baseline"]["paths"][0]["existed"], true);
    assert_eq!(t2["baseline"]["paths"][0]["sha256"], fp_hex("baseline"));

    // A link (also one to a readable file) and a directory are refused, and no trial is written.
    let link = h.path().join("link");
    symlink(&plain, &link).unwrap();
    let dir = h.path().join("dir");
    fs::create_dir(&dir).unwrap();
    let before = snapshot(h.path());
    for (id, p) in [("t3", &link), ("t4", &dir)] {
        let o = record_trial(h.path(), id, p);
        assert_ne!(o.status.code(), Some(0), "{id}");
        assert!(
            text(&o.stderr).contains("E_STATE_INVALID"),
            "{id}: {}",
            text(&o.stderr)
        );
    }
    assert_eq!(snapshot(h.path()), before, "nothing was recorded");

    // A FIFO is not read (a read would wait for a writer), and a second hard link is refused.
    let fifo = h.path().join("fifo");
    let made = std::process::Command::new("/usr/bin/mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    let twin = h.path().join("twin");
    fs::hard_link(&plain, &twin).unwrap();
    let before = snapshot(h.path());
    for (id, p) in [("t6", &fifo), ("t7", &twin)] {
        let o = record_trial(h.path(), id, p);
        assert_ne!(o.status.code(), Some(0), "{id}");
        assert!(
            text(&o.stderr).contains("E_STATE_INVALID"),
            "{id}: {}",
            text(&o.stderr)
        );
    }
    assert_eq!(snapshot(h.path()), before, "nothing was recorded");

    // An unreadable file is an I/O error, not "did not exist".
    if is_root() {
        println!("SKIPPED: unreadable baseline (runs as root)");
        return;
    }
    let secret = h.path().join("secret");
    fs::write(&secret, "x").unwrap();
    let before = snapshot(&state(h.path()));
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
    let o = record_trial(h.path(), "t5", &secret);
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("E_IO_FAILED"),
        "{}",
        text(&o.stderr)
    );
    assert_eq!(snapshot(&state(h.path())), before, "no trial was recorded");
}

#[test]
fn reconcile_does_not_settle_an_output_that_is_a_link_or_a_directory() {
    for (label, shape) in [("link", 0), ("directory", 1)] {
        let h = bound_home("reconcile-shape");
        let out = out_file(h.path(), "victim");
        let o = with_fail(
            h.path(),
            "after-intent:abort",
            &[
                "__record-op",
                "--resource",
                "core.victim",
                "--output",
                &out,
                "--content",
                "v",
            ],
        );
        assert_eq!(o.status.signal(), Some(6), "{label}");
        // The output now exists, but not as the plain file the intent expected. A link to a
        // file with the expected content is the case a following read would call done.
        if shape == 0 {
            let real = h.path().join("real");
            fs::write(&real, "v").unwrap();
            symlink(&real, &out).unwrap();
        } else {
            fs::create_dir(&out).unwrap();
        }
        let before = snapshot(&state(h.path()));
        let r = run(h.path(), &["__reconcile"]);
        assert_eq!(r.status.code(), Some(0), "{label}: {}", text(&r.stderr));
        let said = text(&r.stdout);
        assert!(said.contains("as Unknown"), "{label}: {said}");
        // The operator can tell why the output was not read.
        assert!(
            said.contains("cannot read an output of")
                && said.contains("link or not a regular file"),
            "{label}: {said}"
        );
        assert!(
            !said.contains("as Done") && !said.contains("as Failed"),
            "{label}: {said}"
        );
        assert_ne!(
            snapshot(&state(h.path())),
            before,
            "{label}: the open entry was journaled"
        );
        assert!(!text(&fs::read(state(h.path()).join("ledger.json")).unwrap()).contains(&out));
    }
}

#[test]
fn recover_does_not_advise_resolving_a_multi_path_entry_as_done() {
    let h = bound_home("multi-path");
    let out = out_file(h.path(), "victim");
    let o = with_fail(
        h.path(),
        "after-intent:abort",
        &[
            "__record-op",
            "--resource",
            "core.victim",
            "--output",
            &out,
            "--content",
            "v",
        ],
    );
    assert_eq!(o.status.signal(), Some(6));
    // One recorded path that shows no clear result (a directory): the advice does not mention
    // the multi-path limit.
    fs::create_dir(&out).unwrap();
    let r = run(h.path(), &["state", "recover"]);
    assert_eq!(r.status.code(), Some(3), "{}", text(&r.stderr));
    assert!(
        !text(&r.stderr).contains("more than one path"),
        "{}",
        text(&r.stderr)
    );
    // Give the intent a second recorded output, as a hand-written journal could.
    let j = state(h.path()).join("journal.jsonl");
    let mut lines: Vec<serde_json::Value> = fs::read_to_string(&j)
        .unwrap()
        .lines()
        .map(|l| json(l.as_bytes()))
        .collect();
    let first = lines[0]["recovery"]["outputs"][0].clone();
    let mut second = first.clone();
    second["path"] = out_file(h.path(), "other").into();
    lines[0]["recovery"]["outputs"]
        .as_array_mut()
        .unwrap()
        .push(second);
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(&j, body).unwrap();

    let r = run(h.path(), &["state", "recover"]);
    assert_eq!(r.status.code(), Some(3), "{}", text(&r.stderr));
    assert!(
        text(&r.stderr).contains("more than one path cannot be resolved as done"),
        "{}",
        text(&r.stderr)
    );
    let id = lines[0]["op_id"].as_str().unwrap();
    let d = run(h.path(), &["state", "resolve", id, "--as", "done"]);
    assert_eq!(d.status.code(), Some(3), "{}", text(&d.stderr));
    assert!(
        text(&d.stderr).contains("E_CONFLICT") && text(&d.stderr).contains("more than one path"),
        "{}",
        text(&d.stderr)
    );
}
