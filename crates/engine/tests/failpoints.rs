//! Failure-injection tests. They need the `failpoints` feature (the CLI test gate enables it).

mod common;

use common::*;
use dfm_engine::{Action, Fail, Store};
use dfm_model::Code;
use dfm_model::state::Stamp;
use std::fs;
use std::os::unix::fs::PermissionsExt;

#[test]
fn a_crashed_init_staging_directory_is_discarded_and_state_is_not_half_built() {
    let h = Home::new();
    let mut s = open(&h);
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
    let mut s2 = Store::open_write(
        sys(&h2),
        &h2.layout(),
        Fail::at("init-before-rename", Action::Error),
    )
    .unwrap();
    assert_eq!(s2.init(&stamp).unwrap_err().code, Code::IoFailed);
    assert!(!h2.0.join(".local/state/dotfiles-manager/state").exists());
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
fn a_stalled_writer_blocks_a_second_one_without_touching_state() {
    let h = Home::new();
    let (s, _) = init(&h);
    drop(s);
    // The holder waits at the failpoint until the test creates a file: no timing guess.
    let release = h.0.join("release");
    let slow = Store::open_write(
        sys(&h),
        &h.layout(),
        Fail::at("journal-before-write", Action::Wait(release.clone())),
    )
    .unwrap();
    let out = h.0.join("out").to_str().unwrap().to_string();
    let before = fs::read(h.0.join(".local/state/dotfiles-manager/state/journal.jsonl")).unwrap();
    std::thread::scope(|sc| {
        let holder = sc.spawn(|| slow.append_journal(&intent("op1", &out)));
        // The holder cannot finish, so this attempt always meets the lock.
        assert_eq!(
            code(Store::open_write(sys(&h), &h.layout(), Fail::none())),
            Code::Locked
        );
        assert_eq!(
            fs::read(h.0.join(".local/state/dotfiles-manager/state/journal.jsonl")).unwrap(),
            before
        );
        fs::write(&release, "go").unwrap();
        holder.join().unwrap().unwrap();
    });
}

#[test]
fn a_failure_after_the_rename_is_unconfirmed_not_a_failed_write() {
    let h = Home::new();
    let (s, stamp) = init(&h);
    let old = s.read_ledger().unwrap();
    drop(s);
    let s = Store::open_write(
        sys(&h),
        &h.layout(),
        Fail::at("ledger-after-rename", Action::Error),
    )
    .unwrap();
    let newer = ledger_with(&stamp, &h, "core.x", "a", 'c');
    assert_eq!(code(s.write_ledger(&newer)), Code::IoUnconfirmed);
    // The new ledger is in place; only its durability is unknown.
    assert_eq!(s.read_ledger().unwrap(), newer);
    assert_ne!(newer, old);
}

#[test]
fn a_mid_init_failure_after_the_journal_leaves_no_state_directory() {
    let h = Home::new();
    let mut s = Store::open_write(
        sys(&h),
        &h.layout(),
        Fail::at("init-journal-after-rename", Action::Error),
    )
    .unwrap();
    let stamp = Stamp::of(&binding("/p/profile.toml", "m5"));
    assert_eq!(code(s.init(&stamp)), Code::IoUnconfirmed);
    assert!(!h.0.join(".local/state/dotfiles-manager/state").exists());
}

/// Waits (bounded) until `f` is true.
fn wait_until(what: &str, f: impl Fn() -> bool) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !f() {
        assert!(
            std::time::Instant::now() < end,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn swapping_state_for_a_symlink_mid_write_cannot_redirect_the_write() {
    use std::os::unix::fs::symlink;
    for (point, name) in [
        ("ledger-after-tmp", "ledger"),
        ("journal-before-write", "journal"),
    ] {
        let h = Home::new();
        let (s, stamp) = init(&h);
        drop(s);
        let foreign = Home::new();
        let root = h.0.join(".local/state/dotfiles-manager");
        let release = h.0.join("release");
        let s = Store::open_write(
            sys(&h),
            &h.layout(),
            Fail::at(point, Action::Wait(release.clone())),
        )
        .unwrap();
        let out = h.0.join("out").to_str().unwrap().to_string();
        let newer = ledger_with(&stamp, &h, "core.x", "a", 'b');
        std::thread::scope(|sc| {
            let writer = sc.spawn(|| {
                if name == "ledger" {
                    s.write_ledger(&newer)
                } else {
                    s.append_journal(&intent("op1", &out))
                }
            });
            // The writer is parked at the failpoint, after all its checks.
            wait_until("the failpoint", || {
                release.with_extension("reached").exists()
            });
            // Swap state/ for a link to a foreign directory.
            fs::rename(root.join("state"), root.join("state.moved")).unwrap();
            symlink(&foreign.0, root.join("state")).unwrap();
            fs::write(&release, "go").unwrap();
            writer.join().unwrap().unwrap();
        });
        assert!(
            fs::read_dir(&foreign.0).unwrap().next().is_none(),
            "{point}: the foreign directory was written"
        );
        // The write landed in the directory that was verified, not behind the link.
        let moved = root.join("state.moved");
        if name == "ledger" {
            let bytes = fs::read(moved.join("ledger.json")).unwrap();
            assert_eq!(
                bytes,
                dfm_model::state::canonical(&newer).unwrap(),
                "{point}"
            );
        } else {
            assert!(
                !fs::read(moved.join("journal.jsonl")).unwrap().is_empty(),
                "{point}"
            );
        }
        let leftovers = fs::read_dir(&moved)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0, "{point}");
        // The next write notices the swap and stops before touching anything.
        assert_eq!(
            code(s.write_ledger(&newer)),
            Code::StorageInvalid,
            "{point}"
        );
        assert_eq!(
            code(s.append_journal(&intent("op2", &out))),
            Code::StorageInvalid
        );
        assert!(
            fs::read_dir(&foreign.0).unwrap().next().is_none(),
            "{point}"
        );
    }
}
