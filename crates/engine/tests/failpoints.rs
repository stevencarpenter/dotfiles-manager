//! Failure-injection tests. They need the `failpoints` feature (the CLI test gate enables it).

mod common;

use common::*;
use dfm_engine::{Action, Fail, Store};
use dfm_model::Code;
use dfm_model::state::Stamp;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

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

#[test]
fn init_stops_if_the_staging_directory_is_replaced_before_the_rename() {
    use std::os::unix::fs::symlink;
    let h = Home::new();
    let release = h.0.join("release");
    let mut s = Store::open_write(
        sys(&h),
        &h.layout(),
        Fail::at("init-before-rename", Action::Wait(release.clone())),
    )
    .unwrap();
    let foreign = Home::new();
    let root = h.0.join(".local/state/dotfiles-manager");
    let stamp = Stamp::of(&binding("/p/profile.toml", "m5"));
    std::thread::scope(|sc| {
        let init = sc.spawn(|| s.init(&stamp));
        wait_until("the failpoint", || {
            release.with_extension("reached").exists()
        });
        fs::rename(root.join("state.next"), root.join("elsewhere")).unwrap();
        symlink(&foreign.0, root.join("state.next")).unwrap();
        fs::write(&release, "go").unwrap();
        let e = init.join().unwrap().unwrap_err();
        assert_eq!(e.code, Code::StorageInvalid);
        // The staging directory existed, so this refusal came after a mutation: exit 1, not 3.
        assert!(e.mutated);
        assert_eq!(e.exit(), dfm_model::Exit::Failed);
    });
    assert!(s.mutated());
    assert!(
        !root.join("state").exists(),
        "the swapped directory was not published"
    );
    assert!(fs::read_dir(&foreign.0).unwrap().next().is_none());
}

#[test]
fn a_refusal_before_anything_was_created_still_exits_3() {
    let h = Home::new();
    // Nothing exists and the caller may not create the root: state loss, a gate, no mutation.
    let e = Store::open_write_with(sys(&h), &h.layout(), Fail::none(), false).unwrap_err();
    assert_eq!(e.code, Code::StateInvalid);
    assert!(!e.mutated);
    assert_eq!(e.exit(), dfm_model::Exit::Blocked);
    assert!(!h.0.join(".local").exists(), "nothing was created");
    // A second writer meets the lock: also before any creation of its own.
    let (first, _) = init(&h);
    let e = Store::open_write(sys(&h), &h.layout(), Fail::none()).unwrap_err();
    assert_eq!(e.exit(), dfm_model::Exit::Blocked);
    assert!(!e.mutated);
    drop(first);
}

#[test]
fn a_new_directory_is_opened_under_a_restrictive_umask() {
    const CHILD: &str = "DFM_UMASK_CHILD";
    const NAME: &str = "a_new_directory_is_opened_under_a_restrictive_umask";
    if let Some(dir) = std::env::var_os(CHILD) {
        // The child runs under umask 0500, which clears the owner's read bit of a new directory.
        let h = Home(dir.into());
        let (s, _) = init(&h);
        drop(s);
        std::mem::forget(h);
        return;
    }
    let h = Home::new();
    let exe = std::env::current_exe().unwrap();
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg("umask 0500; exec \"$0\" --exact \"$1\" --nocapture --test-threads=1")
        .arg(exe)
        .arg(NAME)
        .env(CHILD, &h.0)
        .status()
        .unwrap();
    assert!(status.success(), "init failed under umask 0500: {status:?}");
    for dir in [
        ".local",
        ".local/state",
        ".local/state/dotfiles-manager",
        ".local/state/dotfiles-manager/state",
        ".local/state/dotfiles-manager/state/receipts",
    ] {
        let mode = fs::metadata(h.0.join(dir)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{dir}");
    }
}

#[test]
fn published_state_files_have_one_link_and_a_leftover_staged_name_is_a_stray_finding() {
    let h = Home::new();
    let (s, _) = init(&h);
    let state = h.0.join(".local/state/dotfiles-manager/state");
    // The exclusive rename leaves one name per file and nothing staged.
    for f in ["state.json", "ledger.json", "journal.jsonl", "salt"] {
        assert_eq!(fs::metadata(state.join(f)).unwrap().nlink(), 1, "{f}");
    }
    assert!(
        files_below(&state)
            .iter()
            .all(|p| p.extension().is_none_or(|e| e != "tmp"))
    );
    // A name left by a crash before the rename is reported. It does not stop any read.
    for rel in [".salt.1.2.3.4.tmp", "receipts/.r.1.2.3.4.tmp"] {
        let p = state.join(rel);
        fs::write(&p, "x").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let loaded = s.load();
    assert_eq!(
        loaded.strays,
        [
            ".salt.1.2.3.4.tmp (unexpected)",
            "receipts/.r.1.2.3.4.tmp (interrupted write)"
        ]
    );
    assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
    let salt = s.salt().unwrap().unwrap();
    assert_eq!(salt.len(), 32);
    // A create that finds the name taken reports it, changes nothing and leaves nothing staged.
    // `salt_or_create` returns the existing salt before it publishes, so the create is driven
    // on the directory itself.
    for rel in [".salt.1.2.3.4.tmp", "receipts/.r.1.2.3.4.tmp"] {
        fs::remove_file(state.join(rel)).unwrap();
    }
    let dir = dfm_engine::fsys::Dir::open_home(&sys(&h), &state).unwrap();
    assert!(
        !dir.publish_new("salt", b"other", &Fail::none(), "salt")
            .unwrap()
    );
    assert_eq!(s.salt().unwrap().unwrap(), salt);
    assert!(
        files_below(&state).iter().all(|p| !p
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with('.')),
        "nothing is left staged: {:?}",
        files_below(&state)
    );
}
