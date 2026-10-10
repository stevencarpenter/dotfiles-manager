//! Storage behavior in an isolated HOME: topology, lock, atomic writes, journal, reconciliation.

mod common;

use common::*;
use dfm_engine::{Fail, Probe, Store, verify_identity};
use dfm_model::Code;
use dfm_model::state::{Class, Ledger, LedgerResource, OwnedOutput, Phase, Stamp};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::Path;

#[test]
fn init_builds_a_private_layout_once() {
    let h = Home::new();
    let (mut s, stamp) = init(&h);
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
fn damaged_or_lost_state_is_never_recreated() {
    let h = Home::new();
    let (mut s, stamp) = init(&h);
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
    // The store refuses to write a foreign receipt, so put one there by hand.
    let dir = h.0.join(".local/state/dotfiles-manager/state/receipts");
    let fpath = dir.join(foreign.file_name().unwrap());
    fs::write(&fpath, dfm_model::state::canonical(&foreign).unwrap()).unwrap();
    fs::set_permissions(&fpath, fs::Permissions::from_mode(0o600)).unwrap();
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
    // A writer refuses a mistyped state path when it opens, before any init or effect.
    assert_eq!(
        code(Store::open_write(sys(&h), &h.layout(), Fail::none())),
        Code::StorageInvalid
    );
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
    // The writer refuses on open, so no receipt is ever written through the link.
    assert_eq!(
        code(Store::open_write(sys(&h), &h.layout(), Fail::none())),
        Code::StorageInvalid
    );
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
    let mut s = Store::open_write(sys(&h), &l, Fail::none()).unwrap();
    s.init(&Stamp::of(&binding("/p/profile.toml", "m5")))
        .unwrap();
    assert!(
        h.0.join(".local/state/dotfiles-manager/state/state.json")
            .exists()
    );
}

#[test]
fn a_ledger_for_another_identity_is_refused_on_read_and_write_and_stays_unchanged() {
    let h = Home::new();
    let (s, _) = init(&h);
    let other = Stamp::of(&binding("/p/other.toml", "m5"));
    let before = fs::read(h.0.join(".local/state/dotfiles-manager/state/ledger.json")).unwrap();
    let foreign = ledger_with(&other, &h, "core.x", "a", 'a');
    assert_eq!(code(s.write_ledger(&foreign)), Code::IdentityMismatch);
    let on_disk = h.0.join(".local/state/dotfiles-manager/state/ledger.json");
    assert_eq!(fs::read(&on_disk).unwrap(), before);
    // A foreign-stamped ledger put there by hand is refused on read, and gives no authority.
    fs::write(&on_disk, dfm_model::state::canonical(&foreign).unwrap()).unwrap();
    assert_eq!(code(s.read_ledger()), Code::IdentityMismatch);
    let findings = verify_identity(&s.load(), &binding("/p/profile.toml", "m5"));
    assert!(
        findings
            .iter()
            .any(|f| f.code == Code::IdentityMismatch && f.file == "ledger.json")
    );
    // Receipts and trials are held to the same rule.
    let r = receipt(&other, "run-9", vec![]);
    assert_eq!(code(s.write_receipt(&r)), Code::IdentityMismatch);
    assert!(
        fs::read_dir(h.0.join(".local/state/dotfiles-manager/state/receipts"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn a_hand_written_ledger_cannot_own_paths_outside_home_or_manager_storage() {
    let h = Home::new();
    let (s, stamp) = init(&h);
    let on_disk = h.0.join(".local/state/dotfiles-manager/state/ledger.json");
    let home = h.0.to_str().unwrap().to_string();
    for bad in [
        "/etc/hosts".to_string(),
        format!("{home}/.local/state/dotfiles-manager/state/state.json"),
        format!("{home}/.LOCAL/state/Dotfiles-Manager/state/state.json"),
        format!("{home}/.local/share/dotfiles-manager/toolchains/1.98.1/bin/cargo"),
        format!("{home}/.cache/dotfiles-manager/build"),
        format!("{home}/.local/state"),
        format!("{home}/.config"),
    ] {
        let mut l = ledger_with(&stamp, &h, "core.x", "a", 'a');
        l.resources[0].owned_outputs[0].path = bad.clone();
        fs::write(&on_disk, dfm_model::state::canonical(&l).unwrap()).unwrap();
        fs::set_permissions(&on_disk, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(code(s.read_ledger()), Code::StateInvalid, "{bad}");
        let loaded = s.load();
        assert!(
            loaded.ledger.is_err(),
            "{bad}: no authority is read from it"
        );
        let findings = verify_identity(&loaded, &binding("/p/profile.toml", "m5"));
        assert!(findings.iter().any(|f| f.file == "ledger.json"), "{bad}");
    }
}

#[test]
fn claims_cover_data_cache_ancestors_case_and_symlinked_parents() {
    let h = Home::new();
    let (s, stamp) = init(&h);
    let home = h.0.to_str().unwrap().to_string();
    // A symlink inside HOME that points at the manager's own storage.
    symlink(
        h.0.join(".local/state/dotfiles-manager"),
        h.0.join("sneaky"),
    )
    .unwrap();
    let outside = Home::new();
    symlink(&outside.0, h.0.join("away")).unwrap();
    for bad in [
        format!("{home}/.local/share/dotfiles-manager/toolchains/x"),
        format!("{home}/.cache/dotfiles-manager"),
        format!("{home}/.CONFIG/dotfiles-manager/machine.toml"),
        format!("{home}/.local/state"),
        format!("{home}/sneaky/state/ledger.json"),
        format!("{home}/away/file"),
    ] {
        let mut l = ledger_with(&stamp, &h, "core.x", "a", 'a');
        l.resources[0].owned_outputs[0].path = bad.clone();
        assert_eq!(code(s.write_ledger(&l)), Code::StateInvalid, "{bad}");
        assert_eq!(
            code(s.append_journal(&intent("op1", &bad))),
            Code::StateInvalid,
            "{bad}"
        );
    }
    // The leaf itself may be a symlink: a link is a legitimate claim.
    symlink("/etc/hosts", h.0.join("link")).unwrap();
    let mut l = ledger_with(&stamp, &h, "core.x", "link", 'a');
    s.write_ledger(&l).unwrap();
    l.resources.clear();
    s.write_ledger(&l).unwrap();
    assert!(fs::read_dir(&outside.0).unwrap().next().is_none());
}

#[test]
fn the_holder_stops_when_its_lock_file_is_removed_or_replaced() {
    let h = Home::new();
    let (s, stamp) = init(&h);
    let lock = h.0.join(".local/state/dotfiles-manager.lock");
    fs::remove_file(&lock).unwrap();
    // The old holder is fenced: it refuses every further write.
    assert_eq!(
        code(s.write_ledger(&ledger_with(&stamp, &h, "core.x", "a", 'a'))),
        Code::Locked
    );
    assert_eq!(
        code(s.append_journal(&intent("op1", h.0.join("o").to_str().unwrap()))),
        Code::Locked
    );
    drop(s);
    // A replaced file is caught the same way.
    let s = open(&h);
    fs::remove_file(&lock).unwrap();
    fs::write(&lock, "x").unwrap();
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        code(s.write_ledger(&ledger_with(&stamp, &h, "core.x", "a", 'a'))),
        Code::Locked
    );
}

#[test]
fn a_planted_fifo_or_loose_lock_file_cannot_hang_or_be_trusted() {
    let h = Home::new();
    let parent = h.0.join(".local/state");
    fs::create_dir_all(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    let lock = parent.join("dotfiles-manager.lock");
    assert!(
        std::process::Command::new("/usr/bin/mkfifo")
            .arg(&lock)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        code(Store::open_write(sys(&h), &h.layout(), Fail::none())),
        Code::StorageInvalid
    );
    fs::remove_file(&lock).unwrap();
    fs::write(&lock, "").unwrap();
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(
        code(Store::open_write(sys(&h), &h.layout(), Fail::none())),
        Code::StorageInvalid
    );
}

#[test]
fn the_directory_chain_is_checked_again_just_before_each_write() {
    let h = Home::new();
    let (s, stamp) = init(&h);
    let foreign = Home::new();
    let state = h.0.join(".local/state/dotfiles-manager/state");
    // Swap `receipts` for a link after the store was opened and validated.
    fs::remove_dir(state.join("receipts")).unwrap();
    symlink(&foreign.0, state.join("receipts")).unwrap();
    assert_eq!(
        code(s.write_receipt(&receipt(&stamp, "run-1", vec![]))),
        Code::StorageInvalid
    );
    assert!(fs::read_dir(&foreign.0).unwrap().next().is_none());
    // The same for the state root itself.
    let root = h.0.join(".local/state/dotfiles-manager");
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        code(s.write_ledger(&ledger_with(&stamp, &h, "core.x", "a", 'a'))),
        Code::StorageInvalid
    );
    assert_eq!(
        code(s.append_journal(&intent("op1", h.0.join("o").to_str().unwrap()))),
        Code::StorageInvalid
    );
}

#[test]
fn load_reports_listing_errors_odd_names_and_stray_snapshot_files() {
    let h = Home::new();
    let (s, _) = init(&h);
    let state = h.0.join(".local/state/dotfiles-manager/state");
    s.write_snapshot("run-1", "core-x", b"old", Class::Normal)
        .unwrap();
    fs::write(state.join("snapshots/run-1/.core-y.1.2.3.tmp"), "half").unwrap();
    let l = s.load();
    assert!(
        l.strays
            .iter()
            .any(|x| x.contains("snapshots/run-1/.core-y")),
        "{:?}",
        l.strays
    );
    // An unreadable directory is a finding, not an empty listing.
    fs::set_permissions(state.join("trials"), fs::Permissions::from_mode(0o000)).unwrap();
    let l = s.load();
    assert!(
        l.problems
            .iter()
            .any(|p| p.file == "trials" && p.detail.contains("cannot list")),
        "{:?}",
        l.problems
    );
    fs::set_permissions(state.join("trials"), fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn secrets_are_redacted_in_text_and_refused_in_names() {
    let h = Home::new();
    let (s, stamp) = init(&h);
    let s = s.with_secrets(vec!["hunter2".into()]);
    let state = h.0.join(".local/state/dotfiles-manager/state");
    let out = h.0.join("out").to_str().unwrap().to_string();
    let mut e = intent("op1", &out);
    e.reason = Some("auth failed for hunter2".into());
    e.token = Some("tok-hunter2".into());
    s.append_journal(&e).unwrap();
    let mut r = receipt(&stamp, "run-1", vec![out.clone()]);
    r.command = "sync hunter2".into();
    r.operations[0].error = Some("bad hunter2".into());
    s.write_receipt(&r).unwrap();
    // Names cannot be redacted without breaking them, so they are refused, and not echoed.
    let named = h.0.join("tok-hunter2").to_str().unwrap().to_string();
    let bad = s.append_journal(&intent("op2", &named)).unwrap_err();
    assert_eq!(bad.code, Code::StateInvalid);
    assert!(!bad.detail.contains("hunter2"));
    let mut l = ledger_with(&stamp, &h, "core.x", "tok-hunter2", 'a');
    assert_eq!(code(s.write_ledger(&l)), Code::StateInvalid);
    l.resources.clear();
    s.write_ledger(&l).unwrap();
    let mut files = vec![state.join("journal.jsonl"), state.join("ledger.json")];
    files.extend(
        fs::read_dir(state.join("receipts"))
            .unwrap()
            .map(|e| e.unwrap().path()),
    );
    for f in files {
        let body = fs::read_to_string(&f).unwrap();
        assert!(!body.contains("hunter2"), "{f:?}");
    }
}

#[test]
fn reconcile_refuses_a_torn_journal_and_leaves_it_alone() {
    let h = Home::new();
    let (s, _) = init(&h);
    let out = h.0.join("out").to_str().unwrap().to_string();
    s.append_journal(&intent("op1", &out)).unwrap();
    s.append_journal(&intent("op2", &out)).unwrap();
    let j =
        h.0.join(".local/state/dotfiles-manager/state/journal.jsonl");
    let mut bytes = fs::read(&j).unwrap();
    bytes.truncate(bytes.len() - 7);
    fs::write(&j, &bytes).unwrap();
    assert_eq!(code(s.reconcile(&|_| Probe::Done)), Code::StateInvalid);
    assert_eq!(fs::read(&j).unwrap(), bytes);
}
