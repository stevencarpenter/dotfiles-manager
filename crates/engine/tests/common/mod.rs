#![allow(dead_code, reason = "each test binary uses a different subset")]
//! Shared helpers for the engine tests.

use dfm_engine::{Fail, Layout, Store, Sys};
use dfm_model::binding::Binding;
use dfm_model::state::{
    BinaryId, Class, JournalEntry, Ledger, LedgerResource, OperationRecord, OwnedOutput, Phase,
    ProfileRef, Receipt, Recovery, RecoveryOutput, RunResult, Stamp, Status, count_statuses,
};
use dfm_model::{Code, Refusal};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

pub struct Home(pub PathBuf);

impl Home {
    pub fn new() -> Home {
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

    pub fn layout(&self) -> Layout {
        layout(&self.0)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn layout(home: &Path) -> Layout {
    Layout {
        data_dir: home.join(".local/share/dotfiles-manager"),
        cache_dir: home.join(".cache/dotfiles-manager"),
        home: home.into(),
        config_dir: home.join(".config/dotfiles-manager"),
        state_root: home.join(".local/state/dotfiles-manager"),
        lock_file: home.join(".local/state/dotfiles-manager.lock"),
    }
}

pub fn sys(home: &Home) -> Sys {
    Sys {
        uid: fs::metadata(&home.0).unwrap().uid(),
    }
}

pub fn binding(profile: &str, id: &str) -> Binding {
    Binding::new(profile, id).unwrap()
}

/// Opens the store for writing right after the previous holder in this process dropped it.
/// Tests run in parallel threads, and another thread may be starting a child process. Between
/// its fork and its exec the child holds a copy of every open descriptor, so a lock that was
/// just released can still look held for a few milliseconds. This waits for that, for 5 s at
/// most, and only on `E_LOCKED`. Every other refusal is returned at once.
pub fn open_write_after_drop(h: &Home, fail: Fail) -> Store {
    let mut tries = 0;
    loop {
        match Store::open_write(sys(h), &h.layout(), fail.clone()) {
            Err(e) if e.code == Code::Locked && tries < 100 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            r => return r.unwrap(),
        }
    }
}

pub fn open(h: &Home) -> Store {
    open_write_after_drop(h, Fail::none())
}

pub fn init(h: &Home) -> (Store, Stamp) {
    let mut s = open(h);
    let stamp = Stamp::of(&binding("/p/profile.toml", "m5"));
    s.init(&stamp).unwrap();
    (s, stamp)
}

pub fn code<T>(r: Result<T, Refusal>) -> Code {
    r.err().expect("expected a refusal").code
}

pub fn files_below(dir: &Path) -> Vec<PathBuf> {
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

pub fn ledger_with(stamp: &Stamp, home: &Home, resource: &str, file: &str, fp: char) -> Ledger {
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
            coowned: None,
        }],
    })
    .unwrap();
    l
}

pub fn intent(op: &str, path: &str) -> JournalEntry {
    JournalEntry {
        schema: dfm_model::state::SCHEMA,
        op_id: "run-1".into(),
        op: op.into(),
        phase: Phase::Intent,
        at: "2026-10-10T00:00:00.000000000Z".into(),
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

pub fn receipt(stamp: &Stamp, op_id: &str, outputs: Vec<String>) -> Receipt {
    let ops = vec![OperationRecord {
        id: "op1".into(),
        resource: "core.x".into(),
        adapter: "fs-link/v1".into(),
        class: Class::Normal,
        status: Status::Updated,
        started_at: Some("2026-10-10T00:00:00.000000000Z".into()),
        finished_at: Some("2026-10-10T00:00:01.000000000Z".into()),
        before: None,
        after: None,
        owned_outputs: outputs,
        error: None,
        reload: None,
    }];
    Receipt {
        schema: dfm_model::state::SCHEMA,
        op_id: op_id.into(),
        command: "sync".into(),
        started_at: "2026-10-10T00:00:00.000000000Z".into(),
        finished_at: "2026-10-10T00:00:02.000000000Z".into(),
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
        approvals: vec![],
        result: RunResult {
            outcome: "ok".into(),
            exit_code: 0,
            counts: count_statuses(&ops),
        },
        operations: ops,
    }
}
