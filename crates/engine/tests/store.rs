//! Storage behavior in an isolated HOME: topology, lock, atomic writes, journal, reconciliation.

use dfm_engine::{Action, Fail, Layout, Probe, Store, Sys, verify_identity};
use dfm_model::binding::Binding;
use dfm_model::state::{
    BinaryId, Class, JournalEntry, Ledger, LedgerResource, OperationRecord, OwnedOutput, Phase,
    ProfileRef, Receipt, Recovery, RecoveryOutput, RunResult, Stamp, Status, count_statuses,
};
use dfm_model::{Code, Refusal};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

struct Home(PathBuf);

impl Home {
    fn new() -> Home {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "dfm-engine-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
        Home(p)
    }

    fn layout(&self) -> Layout {
        layout(&self.0)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn layout(home: &Path) -> Layout {
    Layout {
        home: home.into(),
        config_dir: home.join(".config/dotfiles-manager"),
        state_root: home.join(".local/state/dotfiles-manager"),
        lock_file: home.join(".local/state/dotfiles-manager.lock"),
    }
}

fn sys(home: &Home) -> Sys {
    Sys {
        uid: fs::metadata(&home.0).unwrap().uid(),
        o_nofollow: 0x0100,
        o_nonblock: 0x0004,
        eloop: 62,
    }
}

fn binding(profile: &str, id: &str) -> Binding {
    Binding::new(profile, id).unwrap()
}

fn open(h: &Home) -> Store {
    Store::open_write(sys(h), &h.layout(), Fail::none()).unwrap()
}

fn init(h: &Home) -> (Store, Stamp) {
    let s = open(h);
    let stamp = Stamp::of(&binding("/p/profile.toml", "m5"));
    s.init(&stamp).unwrap();
    (s, stamp)
}

fn code<T>(r: Result<T, Refusal>) -> Code {
    r.err().expect("expected a refusal").code
}

fn files_below(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        out.push(p.clone());
        if e.file_type().unwrap().is_dir() {
            out.extend(files_below(&p));
        }
    }
    out
}

fn ledger_with(stamp: &Stamp, home: &Home, resource: &str, file: &str, fp: char) -> Ledger {
    let mut l = Ledger::empty(stamp.clone());
    l.record(LedgerResource {
        resource: resource.into(),
        adapter: "fs-link/v1".into(),
        class: Class::Normal,
        op_id: "op1".into(),
        owned_outputs: vec![OwnedOutput {
            path: home.0.join(file).to_str().unwrap().into(),
            mode: 0o600,
            fingerprint: Some(fp.to_string().repeat(64)),
        }],
    })
    .unwrap();
    l
}

fn intent(op: &str, path: &str) -> JournalEntry {
    JournalEntry {
        schema: 1,
        op_id: "run-1".into(),
        op: op.into(),
        phase: Phase::Intent,
        at: "2026-10-10T00:00:00Z".into(),
        resource: "core.x".into(),
        adapter: "fs-link/v1".into(),
        class: Class::Normal,
        recovery: Some(Recovery {
            outputs: vec![RecoveryOutput {
                path: path.into(),
                existed_before: false,
                expected_sha256: Some("a".repeat(64)),
            }],
            snapshot: None,
        }),
        token: None,
        reason: None,
    }
}

fn receipt(stamp: &Stamp, op_id: &str, outputs: Vec<String>) -> Receipt {
    let ops = vec![OperationRecord {
        id: "op1".into(),
        resource: "core.x".into(),
        adapter: "fs-link/v1".into(),
        class: Class::Normal,
        status: Status::Updated,
        started_at: "2026-10-10T00:00:00Z".into(),
        finished_at: "2026-10-10T00:00:01Z".into(),
        before: None,
        after: None,
        owned_outputs: outputs,
        error: None,
    }];
    Receipt {
        schema: 1,
        op_id: op_id.into(),
        command: "sync".into(),
        started_at: "2026-10-10T00:00:00Z".into(),
        finished_at: "2026-10-10T00:00:02Z".into(),
        binary: BinaryId {
            version: "0.1.0".into(),
            commit: "unknown".into(),
            sha256: "0".repeat(64),
        },
        profile: ProfileRef {
            id: stamp.profile.clone(),
            binding_digest: stamp.binding_digest.clone(),
        },
        machine_id: stamp.machine_id.clone(),
        plan_digest: None,
        approval: None,
        sources: vec![],
        result: RunResult {
            outcome: "ok".into(),
            exit_code: 0,
            counts: count_statuses(&ops),
        },
        operations: ops,
    }
}

#[test]
fn init_builds_a_private_layout_once() {
    let h = Home::new();
    let (s, stamp) = init(&h);
    let state = h.0.join(".local/state/dotfiles-manager/state");
    for (p, mode) in [
        (state.clone(), 0o700),
        (state.join("receipts"), 0o700),
        (state.join("snapshots"), 0o700),
        (state.join("trials"), 0o700),
        (state.join("state.json"), 0o600),
        (state.join("ledger.json"), 0o600),
        (state.join("journal.jsonl"), 0o600),
    ] {
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o777, mode, "{p:?}");
    }
    assert_eq!(s.read_state().unwrap().binding, stamp);
    assert!(s.read_ledger().unwrap().resources.is_empty());
    assert_eq!(s.init(&stamp).unwrap_err().code, Code::StateInvalid);
}

#[test]
fn a_crashed_init_staging_directory_is_discarded_and_state_is_not_half_built() {
    let h = Home::new();
    let s = open(&h);
    // Leftover from an init that died before the rename.
    let staging = h.0.join(".local/state/dotfiles-manager/state.next");
    fs::create_dir_all(staging.join("receipts")).unwrap();
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(staging.join("receipts"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(staging.join("state.json"), "partial").unwrap();
    let stamp = Stamp::of(&binding("/p/profile.toml", "m5"));
    s.init(&stamp).unwrap();
    assert!(!staging.exists());
    assert_eq!(s.read_state().unwrap().binding, stamp);

    // A failure before the rename leaves no `state` directory at all.
    let h2 = Home::new();
    let s2 = Store::open_write(
        sys(&h2),
        &h2.layout(),
        Fail::at("init-before-rename", Action::Error),
    )
    .unwrap();
    assert_eq!(s2.init(&stamp).unwrap_err().code, Code::IoFailed);
    assert!(!h2.0.join(".local/state/dotfiles-manager/state").exists());
}

#[test]
fn damaged_or_lost_state_is_never_recreated() {
    let h = Home::new();
    let (s, stamp) = init(&h);
    let state = h.0.join(".local/state/dotfiles-manager/state");
    fs::remove_file(state.join("state.json")).unwrap();
    assert_eq!(s.init(&stamp).unwrap_err().code, Code::StateInvalid);
    assert_eq!(s.read_state().unwrap_err().code, Code::StateInvalid);
    assert!(!state.join("state.json").exists(), "nothing was rebuilt");
}

#[test]
fn the_writer_lock_is_exclusive_and_released_on_drop() {
    let h = Home::new();
    let first = open(&h);
    assert_eq!(
        code(Store::open_write(sys(&h), &h.layout(), Fail::none())),
        Code::Locked
    );
    // Readers take no lock.
    assert!(Store::open_read(sys(&h), &h.layout(), Fail::none()).is_ok());
    drop(first);
    open(&h);
}

#[test]
fn a_failed_ledger_write_keeps_the_last_good_ledger_and_leaves_no_temporary_file() {
    let h = Home::new();
    let (s, stamp) = init(&h);
    let good = ledger_with(&stamp, &h, "core.x", "a", 'a');
    s.write_ledger(&good).unwrap();
    drop(s);
    for point in ["ledger-before-write", "ledger-after-tmp"] {
        let s = Store::open_write(sys(&h), &h.layout(), Fail::at(point, Action::Error)).unwrap();
        let newer = ledger_with(&stamp, &h, "core.x", "a", 'b');
        assert_eq!(code(s.write_ledger(&newer)), Code::IoFailed, "{point}");
        assert_eq!(s.read_ledger().unwrap(), good, "{point}");
        let state = h.0.join(".local/state/dotfiles-manager/state");
        let temps: Vec<_> = files_below(&state)
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "tmp"))
            .collect();
        assert!(temps.is_empty(), "{point}: {temps:?}");
    }
}

#[test]
fn receipts_are_immutable_and_must_match_their_name() {
    let h = Home::new();
    let (s, stamp) = init(&h);
    let r = receipt(&stamp, "run-1", vec![]);
    let name = s.write_receipt(&r).unwrap();
    assert_eq!(name, "20261010T000000Z-run-1.json");
    assert_eq!(code(s.write_receipt(&r)), Code::StorageInvalid);
    let loaded = s.load();
    assert_eq!(loaded.receipts.len(), 1);
    assert_eq!(loaded.receipts[0].1.as_ref().unwrap(), &r);

    let path =
        h.0.join(".local/state/dotfiles-manager/state/receipts")
            .join(&name);
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    fs::rename(&path, path.with_file_name("20261010T000000Z-other.json")).unwrap();
    let l = s.load();
    assert_eq!(
        l.receipts[0].1.as_ref().unwrap_err().code,
        Code::StateInvalid
    );
}

#[test]
fn claims_outside_home_or_inside_manager_storage_are_refused() {
    let h = Home::new();
    let (s, stamp) = init(&h);
    let home = h.0.to_str().unwrap();
    for bad in [
        "/etc/hosts".to_string(),
        home.to_string(),
        format!("{home}/.local/state/dotfiles-manager/state/ledger.json"),
        format!("{home}/.config/dotfiles-manager/machine.toml"),
        format!("{home}/.local/state/dotfiles-manager.lock"),
    ] {
        let mut l = Ledger::empty(stamp.clone());
        l.resources.push(LedgerResource {
            resource: "r".into(),
            adapter: "a".into(),
            class: Class::Normal,
            op_id: "op1".into(),
            owned_outputs: vec![OwnedOutput {
                path: bad.clone(),
                mode: 0o600,
                fingerprint: Some("a".repeat(64)),
            }],
        });
        assert_eq!(code(s.write_ledger(&l)), Code::StateInvalid, "{bad}");
        assert_eq!(
            code(s.append_journal(&intent("op1", &bad))),
            Code::StateInvalid
        );
    }
    assert!(s.read_ledger().unwrap().resources.is_empty());
}

#[test]
fn snapshots_are_private_scoped_and_never_hold_sensitive_content() {
    let h = Home::new();
    let (s, _) = init(&h);
    s.write_snapshot("run-1", "core-x", b"old", Class::Normal)
        .unwrap();
    let p =
        h.0.join(".local/state/dotfiles-manager/state/snapshots/run-1");
    assert_eq!(fs::metadata(&p).unwrap().mode() & 0o777, 0o700);
    assert_eq!(
        fs::metadata(p.join("core-x")).unwrap().mode() & 0o777,
        0o600
    );
    assert_eq!(
        code(s.write_snapshot("run-1", "secret", b"hunter2", Class::Sensitive)),
        Code::StorageInvalid
    );
    assert_eq!(
        code(s.write_snapshot("../run-1", "x", b"x", Class::Normal)),
        Code::StorageInvalid
    );
    assert!(!p.join("secret").exists());
}

#[test]
fn an_interrupted_operation_is_reconciled_without_replay_or_ownership() {
    let h = Home::new();
    let (s, stamp) = init(&h);
    let out = h.0.join("out").to_str().unwrap().to_string();
    let ledger = s.read_ledger().unwrap();
    s.append_journal(&intent("op1", &out)).unwrap();
    s.append_journal(&intent("op2", &out)).unwrap();
    let l = s.load();
    let findings = verify_identity(&l, &binding("/p/profile.toml", "m5"));
    assert_eq!(
        findings
            .iter()
            .filter(|f| f.code == Code::OpInterrupted)
            .count(),
        2
    );

    let done = s
        .reconcile(&|e| {
            if e.op == "op1" {
                Probe::Done
            } else {
                Probe::Unknown
            }
        })
        .unwrap();
    assert_eq!(
        done.iter().map(|d| (d.1.as_str(), d.2)).collect::<Vec<_>>(),
        [("op1", Phase::Done), ("op2", Phase::Unknown)]
    );
    assert!(
        s.reconcile(&|_| Probe::Done).unwrap().is_empty(),
        "nothing left to settle"
    );
    assert_eq!(
        s.read_ledger().unwrap(),
        ledger,
        "reconciliation never edits the ledger"
    );
    assert!(
        !s.read_ledger()
            .unwrap()
            .authorizes_delete("core.x", &out, &"a".repeat(64))
    );
    assert!(
        !out.is_empty() && !Path::new(&out).exists(),
        "nothing was replayed"
    );
    let _ = stamp;
}

#[test]
fn a_torn_journal_line_is_reported_and_never_extended() {
    let h = Home::new();
    let (s, _) = init(&h);
    let out = h.0.join("out").to_str().unwrap().to_string();
    s.append_journal(&intent("op1", &out)).unwrap();
    let j =
        h.0.join(".local/state/dotfiles-manager/state/journal.jsonl");
    let mut bytes = fs::read(&j).unwrap();
    bytes.truncate(bytes.len() - 10);
    fs::write(&j, &bytes).unwrap();
    assert_eq!(code(s.read_journal()), Code::StateInvalid);
    assert_eq!(
        code(s.append_journal(&intent("op2", &out))),
        Code::StateInvalid
    );
    assert_eq!(
        fs::read(&j).unwrap(),
        bytes,
        "the damaged journal was not touched"
    );
}

#[test]
fn identity_mismatches_and_unreadable_documents_are_findings() {
    let h = Home::new();
    let (s, stamp) = init(&h);
    s.write_receipt(&receipt(&stamp, "run-1", vec![])).unwrap();
    let other = Stamp::of(&binding("/p/other.toml", "m5"));
    let mut foreign = receipt(&other, "run-2", vec![]);
    foreign.started_at = "2026-10-11T00:00:00Z".into();
    s.write_receipt(&foreign).unwrap();
    let dir = h.0.join(".local/state/dotfiles-manager/state/receipts");
    fs::write(dir.join("bad.json"), "{").unwrap();
    fs::set_permissions(dir.join("bad.json"), fs::Permissions::from_mode(0o600)).unwrap();
    let findings = verify_identity(&s.load(), &binding("/p/profile.toml", "m5"));
    let by = |c| findings.iter().filter(|f| f.code == c).collect::<Vec<_>>();
    assert_eq!(by(Code::IdentityMismatch).len(), 1, "{findings:?}");
    assert!(by(Code::IdentityMismatch)[0].file.contains("run-2"));
    assert_eq!(by(Code::StateInvalid).len(), 1, "{findings:?}");
    assert_eq!(findings.len(), 2);
}

#[test]
fn redirected_roots_and_conflicting_types_are_refused_with_zero_foreign_writes() {
    let foreign = Home::new();
    for victim in [".local/state/dotfiles-manager", ".local/state", ".local"] {
        let h = Home::new();
        let link = h.0.join(victim);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        fs::set_permissions(link.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&foreign.0, &link).unwrap();
        let r = Store::open_write(sys(&h), &h.layout(), Fail::none());
        assert_eq!(code(r), Code::StorageInvalid, "{victim}");
        assert!(
            files_below(&foreign.0).is_empty(),
            "{victim}: foreign dir was written"
        );
    }
    // A file where a directory belongs, and a symlinked state or receipts directory.
    let h = Home::new();
    let root = h.0.join(".local/state/dotfiles-manager");
    fs::create_dir_all(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(root.join("state"), "file").unwrap();
    let stamp = Stamp::of(&binding("/p/profile.toml", "m5"));
    let s = open(&h);
    assert_eq!(code(s.init(&stamp)), Code::StorageInvalid);
    drop(s);
    fs::remove_file(root.join("state")).unwrap();
    let (s, _) = init(&h);
    drop(s);
    let rec = root.join("state/receipts");
    fs::remove_dir(&rec).unwrap();
    symlink(&foreign.0, &rec).unwrap();
    assert_eq!(
        code(Store::open_read(sys(&h), &h.layout(), Fail::none())),
        Code::StorageInvalid
    );
    let s = open(&h);
    let r = receipt(&stamp, "run-1", vec![]);
    assert_eq!(code(s.write_receipt(&r)), Code::StorageInvalid);
    assert!(files_below(&foreign.0).is_empty());
}

#[test]
fn permissions_and_owners_are_checked_not_repaired() {
    let h = Home::new();
    let (s, _) = init(&h);
    drop(s);
    let root = h.0.join(".local/state/dotfiles-manager");
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        code(Store::open_read(sys(&h), &h.layout(), Fail::none())),
        Code::StorageInvalid
    );
    assert_eq!(
        fs::metadata(&root).unwrap().mode() & 0o777,
        0o755,
        "not repaired"
    );
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let ledger = root.join("state/ledger.json");
    fs::set_permissions(&ledger, fs::Permissions::from_mode(0o644)).unwrap();
    let s = Store::open_read(sys(&h), &h.layout(), Fail::none())
        .unwrap()
        .unwrap();
    assert_eq!(code(s.read_ledger()), Code::StorageInvalid);
    // A wrong owner is a refusal too.
    let mut other = sys(&h);
    other.uid += 1;
    assert_eq!(
        code(Store::open_read(other, &h.layout(), Fail::none())),
        Code::StorageInvalid
    );
}

#[test]
fn a_macos_style_alias_of_home_is_recognized() {
    let h = Home::new();
    let alias_dir = Home::new();
    let alias = alias_dir.0.join("home-alias");
    symlink(&h.0, &alias).unwrap();
    let l = layout(&alias);
    let s = Store::open_write(sys(&h), &l, Fail::none()).unwrap();
    s.init(&Stamp::of(&binding("/p/profile.toml", "m5")))
        .unwrap();
    assert!(
        h.0.join(".local/state/dotfiles-manager/state/state.json")
            .exists()
    );
}

#[test]
fn a_stalled_writer_blocks_a_second_one_without_touching_state() {
    let h = Home::new();
    let (s, _) = init(&h);
    drop(s);
    let slow = Store::open_write(
        sys(&h),
        &h.layout(),
        Fail::at(
            "journal-before-write",
            Action::Sleep(Duration::from_millis(300)),
        ),
    )
    .unwrap();
    let out = h.0.join("out").to_str().unwrap().to_string();
    std::thread::scope(|sc| {
        sc.spawn(|| slow.append_journal(&intent("op1", &out)).unwrap());
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            code(Store::open_write(sys(&h), &h.layout(), Fail::none())),
            Code::Locked
        );
    });
}
