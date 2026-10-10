//! The state directory: initialization, journal, ledger, receipts, trials and snapshots.

use crate::fail::Fail;
use crate::fsys::{
    self, Found, Layout, Lock, Sys, append_line, below, canonical_home, check_dir_chain,
    create_empty, publish_new, read_private, replace_file, storage,
};
use dfm_model::binding::Binding;
use dfm_model::state::{
    self, Class, JournalEntry, Ledger, Phase, Receipt, Stamp, StateDoc, TrialRecord, canonical,
};
use dfm_model::{Code, Refusal};
use std::fs;
use std::path::{Path, PathBuf};

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

const STATE: &str = "state";
const STAGING: &str = "state.next";
const SUBDIRS: [&str; 3] = ["receipts", "snapshots", "trials"];

/// What a probe learned about an interrupted operation (its `already_done` check).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    Done,
    NotDone,
    Unknown,
}

/// A validated state directory. A value opened for writing holds the manager lock until it is
/// dropped.
#[derive(Debug)]
pub struct Store {
    sys: Sys,
    home: PathBuf,
    root: PathBuf,
    dir: PathBuf,
    reserved: Vec<PathBuf>,
    fail: Fail,
    lock: Option<Lock>,
}

impl Store {
    fn new(sys: Sys, layout: &Layout, home: PathBuf, fail: Fail) -> Result<Store, Refusal> {
        let join = |p: &Path| -> Result<PathBuf, Refusal> {
            Ok(below(&layout.home, p)?
                .iter()
                .fold(home.clone(), |a, c| a.join(c)))
        };
        let root = join(&layout.state_root)?;
        Ok(Store {
            sys,
            dir: root.join(STATE),
            reserved: vec![
                root.clone(),
                join(&layout.config_dir)?,
                join(&layout.lock_file)?,
            ],
            root,
            home,
            fail,
            lock: None,
        })
    }

    fn root_rel(layout: &Layout) -> Result<Vec<String>, Refusal> {
        let rel = below(&layout.home, &layout.state_root)?;
        if rel.is_empty() {
            return Err(storage("the state root is HOME itself"));
        }
        Ok(rel)
    }

    /// Opens for reading without taking the lock and without creating anything. `None` when no
    /// state directory exists. A redirected, mistyped or badly owned path is a refusal.
    pub fn open_read(sys: Sys, layout: &Layout, fail: Fail) -> Result<Option<Store>, Refusal> {
        let home = canonical_home(&sys, &layout.home)?;
        let rel = Self::root_rel(layout)?;
        let private_from = rel.len() - 1;
        if check_dir_chain(&sys, &home, &rel, private_from, false)? == Found::Absent {
            return Ok(None);
        }
        let s = Store::new(sys, layout, home, fail)?;
        Ok(s.check_layout()?.then_some(s))
    }

    /// Validates `state` and its subdirectories if they exist. `false` when `state` is absent.
    fn check_layout(&self) -> Result<bool, Refusal> {
        if check_dir_chain(&self.sys, &self.root, &[STATE.into()], 0, false)? == Found::Absent {
            return Ok(false);
        }
        for d in SUBDIRS {
            check_dir_chain(&self.sys, &self.dir, &[d.into()], 0, false)?;
        }
        Ok(true)
    }

    /// Opens for writing: creates the shared parent of the lock file, takes the exclusive lock
    /// (`E_LOCKED` if held, never waiting), then validates or creates the manager root.
    pub fn open_write(sys: Sys, layout: &Layout, fail: Fail) -> Result<Store, Refusal> {
        let home = canonical_home(&sys, &layout.home)?;
        let lock_rel = below(&layout.home, &layout.lock_file)?;
        let parent = &lock_rel[..lock_rel.len().saturating_sub(1)];
        check_dir_chain(&sys, &home, parent, parent.len(), true)?;
        let lock_path = lock_rel.iter().fold(home.clone(), |a, c| a.join(c));
        let lock = Lock::acquire(&sys, &lock_path)?;
        let rel = Self::root_rel(layout)?;
        check_dir_chain(&sys, &home, &rel, rel.len() - 1, true)?;
        let mut s = Store::new(sys, layout, home, fail)?;
        s.lock = Some(lock);
        // A writer must not act on a state directory whose layout it would refuse to read.
        s.check_layout()?;
        Ok(s)
    }

    fn need_lock(&self) -> Result<(), Refusal> {
        if self.lock.is_some() {
            Ok(())
        } else {
            Err(storage("a write needs the manager lock"))
        }
    }

    /// The state directory exists (and passed topology validation).
    pub fn is_initialized(&self) -> Result<bool, Refusal> {
        Ok(check_dir_chain(&self.sys, &self.root, &[STATE.into()], 0, false)? == Found::Present)
    }

    /// Creates the state directory for `stamp`. It builds `state.next` and renames it into place,
    /// so a crash leaves either no `state` or a complete one. An existing `state` that is not an
    /// empty directory is refused and left alone: lost or damaged state is never recreated.
    pub fn init(&self, stamp: &Stamp) -> Result<(), Refusal> {
        self.need_lock()?;
        let (sys, dir) = (&self.sys, &self.dir);
        if self.is_initialized()? {
            let empty = fs::read_dir(dir)
                .map_err(|e| fsys::io("list state", &e))?
                .next()
                .is_none();
            if !empty {
                return Err(Refusal::new(
                    Code::StateInvalid,
                    format!(
                        "{} exists and is not empty; it is not recreated",
                        dir.display()
                    ),
                ));
            }
        }
        let staging = self.root.join(STAGING);
        if check_dir_chain(sys, &self.root, &[STAGING.into()], 0, false)? == Found::Present {
            // Our own half-built directory from a crashed init. It never held authority.
            fs::remove_dir_all(&staging).map_err(|e| fsys::io("discard staging", &e))?;
        }
        check_dir_chain(sys, &self.root, &[STAGING.into()], 0, true)?;
        for d in SUBDIRS {
            check_dir_chain(sys, &staging, &[d.into()], 0, true)?;
        }
        let doc = StateDoc::new(stamp.clone(), now_unix());
        publish_new(
            &staging,
            "state.json",
            &canonical(&doc),
            &self.fail,
            "init-state",
        )?;
        publish_new(
            &staging,
            "ledger.json",
            &canonical(&Ledger::empty(stamp.clone())),
            &self.fail,
            "init-ledger",
        )?;
        create_empty(&staging.join("journal.jsonl"))?;
        self.fail.hit("init-before-rename")?;
        fs::rename(&staging, dir).map_err(|e| fsys::io("publish state", &e))?;
        fs::File::open(&self.root)
            .and_then(|d| d.sync_all())
            .map_err(|e| fsys::io("sync state root", &e))
    }

    fn read_doc(&self, name: &str) -> Result<Vec<u8>, Refusal> {
        read_private(&self.sys, &self.dir.join(name), state::MAX_BYTES)?.ok_or_else(|| {
            Refusal::new(
                Code::StateInvalid,
                format!("{name} is missing; it is not reconstructed"),
            )
        })
    }

    pub fn read_state(&self) -> Result<StateDoc, Refusal> {
        StateDoc::parse(&self.read_doc("state.json")?)
    }

    /// The state doc, and a refusal unless it was written for this binding's identity.
    pub fn require_identity(&self, b: &Binding) -> Result<StateDoc, Refusal> {
        let doc = self.read_state()?;
        if doc.binding.matches(b) {
            Ok(doc)
        } else {
            Err(Refusal::new(
                Code::IdentityMismatch,
                format!(
                    "state belongs to {} as {}, not {} as {}",
                    doc.binding.profile,
                    doc.binding.machine_id,
                    b.profile(),
                    b.machine_id()
                ),
            ))
        }
    }

    pub fn read_ledger(&self) -> Result<Ledger, Refusal> {
        Ledger::parse(&self.read_doc("ledger.json")?)
    }

    pub fn read_journal(&self) -> Result<Vec<JournalEntry>, Refusal> {
        state::parse_journal(&self.read_doc("journal.jsonl")?)
    }

    /// A claim must lie inside HOME and outside the manager's own storage and configuration.
    /// Whether a path in HOME belongs to the manager is the adapter's evidence, not a guess.
    fn check_claim(&self, path: &str) -> Result<(), Refusal> {
        let p = Path::new(path);
        let ok = state::valid_path(path)
            && p.starts_with(&self.home)
            && p != self.home
            && !self.reserved.iter().any(|r| p.starts_with(r));
        if ok {
            Ok(())
        } else {
            Err(Refusal::new(
                Code::StateInvalid,
                format!("{path} is outside HOME or inside the manager's own storage"),
            ))
        }
    }

    /// Appends one journal entry and syncs it. Intent entries are written before a side effect.
    pub fn append_journal(&self, e: &JournalEntry) -> Result<(), Refusal> {
        self.need_lock()?;
        for o in e.recovery.iter().flat_map(|r| &r.outputs) {
            self.check_claim(&o.path)?;
        }
        append_line(
            &self.sys,
            &self.dir.join("journal.jsonl"),
            &e.to_line()?,
            &self.fail,
            "journal",
        )
    }

    /// Replaces the ledger atomically. A failure leaves the previous ledger in place.
    pub fn write_ledger(&self, l: &Ledger) -> Result<(), Refusal> {
        self.need_lock()?;
        l.validate()?;
        for o in l.resources.iter().flat_map(|r| &r.owned_outputs) {
            self.check_claim(&o.path)?;
        }
        replace_file(
            &self.dir,
            "ledger.json",
            &canonical(l),
            &self.fail,
            "ledger",
        )
    }

    /// Publishes a receipt. Receipts are immutable: an existing name is a refusal, never an
    /// overwrite. Returns the file name.
    pub fn write_receipt(&self, r: &Receipt) -> Result<String, Refusal> {
        self.need_lock()?;
        r.validate()?;
        for p in r.operations.iter().flat_map(|o| &o.owned_outputs) {
            self.check_claim(p)?;
        }
        let name = r
            .file_name()
            .ok_or_else(|| Refusal::new(Code::StateInvalid, "receipt has no usable time"))?;
        let dir = self.dir.join("receipts");
        check_dir_chain(&self.sys, &self.dir, &["receipts".into()], 0, false)?;
        if publish_new(&dir, &name, &canonical(r), &self.fail, "receipt")? {
            Ok(name)
        } else {
            Err(storage(format!(
                "receipt {name} already exists; receipts are immutable"
            )))
        }
    }

    /// Replaces an active trial record atomically.
    pub fn write_trial(&self, t: &TrialRecord) -> Result<(), Refusal> {
        self.need_lock()?;
        t.validate()?;
        for p in t
            .owned_outputs
            .iter()
            .chain(t.baseline.paths.iter().map(|b| &b.path))
        {
            self.check_claim(p)?;
        }
        check_dir_chain(&self.sys, &self.dir, &["trials".into()], 0, false)?;
        replace_file(
            &self.dir.join("trials"),
            &format!("{}.json", t.trial_id),
            &canonical(t),
            &self.fail,
            "trial",
        )
    }

    /// Stores restorable content for an operation. Only the normal class is allowed: a sensitive
    /// resource has no snapshot content.
    pub fn write_snapshot(
        &self,
        op_id: &str,
        name: &str,
        bytes: &[u8],
        class: Class,
    ) -> Result<(), Refusal> {
        self.need_lock()?;
        if class == Class::Sensitive {
            return Err(storage("a sensitive resource has no snapshot content"));
        }
        if !state::valid_id(op_id) || !state::valid_id(name) {
            return Err(storage("snapshot names must be plain ids"));
        }
        let rel = ["snapshots".to_string(), op_id.to_string()];
        check_dir_chain(&self.sys, &self.dir, &rel, 0, true)?;
        let dir = self.dir.join("snapshots").join(op_id);
        if publish_new(&dir, name, bytes, &self.fail, "snapshot")? {
            Ok(())
        } else {
            Err(storage(format!("snapshot {op_id}/{name} already exists")))
        }
    }

    /// Settles each interrupted operation by asking `probe` (the operation's `already_done`
    /// check) and journaling the answer. It never runs an operation again, never edits the ledger
    /// and never creates ownership or delete authority: a finished operation becomes owned only
    /// when a normal run records it. Returns `(op_id, op, phase)` for each entry written.
    pub fn reconcile(
        &self,
        probe: &dyn Fn(&JournalEntry) -> Probe,
    ) -> Result<Vec<(String, String, Phase)>, Refusal> {
        self.need_lock()?;
        let entries = self.read_journal()?;
        let mut out = Vec::new();
        for i in state::unfinished(&entries) {
            let (phase, why) = match probe(i) {
                Probe::Done => (Phase::Done, "reconciled: the effect is present"),
                Probe::NotDone => (Phase::Failed, "reconciled: the effect is absent"),
                Probe::Unknown => (Phase::Unknown, "reconciled: the effect cannot be observed"),
            };
            let e = JournalEntry {
                phase,
                at: state::rfc3339(now_unix()),
                recovery: None,
                token: None,
                reason: Some(why.into()),
                ..i.clone()
            };
            self.append_journal(&e)?;
            out.push((e.op_id, e.op, phase));
        }
        Ok(out)
    }

    /// Reads every document. Each file's result is kept separately so one unreadable document
    /// neither hides nor changes the others.
    pub fn load(&self) -> Loaded {
        let list = |dir: &str| -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(self.dir.join(dir))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect();
            names.sort();
            names
        };
        let mut strays = Vec::new();
        let mut receipts = Vec::new();
        for n in list("receipts") {
            if n.starts_with('.') && n.ends_with(".tmp") {
                strays.push(format!("receipts/{n} (interrupted write)"));
                continue;
            }
            let r = read_private(
                &self.sys,
                &self.dir.join("receipts").join(&n),
                state::MAX_BYTES,
            )
            .and_then(|b| b.ok_or_else(|| storage("vanished")))
            .and_then(|b| Receipt::parse(&b))
            .and_then(|r| {
                if r.file_name().as_deref() == Some(&n) {
                    Ok(r)
                } else {
                    Err(Refusal::new(
                        Code::StateInvalid,
                        "name does not match content",
                    ))
                }
            });
            receipts.push((n, r));
        }
        let mut trials = Vec::new();
        for n in list("trials") {
            if n.starts_with('.') && n.ends_with(".tmp") {
                strays.push(format!("trials/{n} (interrupted write)"));
                continue;
            }
            let t = read_private(
                &self.sys,
                &self.dir.join("trials").join(&n),
                state::MAX_BYTES,
            )
            .and_then(|b| b.ok_or_else(|| storage("vanished")))
            .and_then(|b| TrialRecord::parse(&b))
            .and_then(|t| {
                if format!("{}.json", t.trial_id) == n {
                    Ok(t)
                } else {
                    Err(Refusal::new(
                        Code::StateInvalid,
                        "name does not match content",
                    ))
                }
            });
            trials.push((n, t));
        }
        for n in list("") {
            let known = ["state.json", "ledger.json", "journal.jsonl"];
            if !known.contains(&n.as_str()) && !SUBDIRS.contains(&n.as_str()) {
                strays.push(format!("{n} (unexpected)"));
            }
        }
        if let Ok(rd) = fs::read_dir(&self.root) {
            for e in rd.flatten() {
                if let Ok(n) = e.file_name().into_string()
                    && n != STATE
                {
                    strays.push(format!("../{n} (staging or previous state)"));
                }
            }
        }
        strays.sort();
        Loaded {
            state: self.read_state(),
            ledger: self.read_ledger(),
            journal: self.read_journal(),
            receipts,
            trials,
            strays,
        }
    }
}

/// Every state document, each with its own result.
#[derive(Debug)]
pub struct Loaded {
    pub state: Result<StateDoc, Refusal>,
    pub ledger: Result<Ledger, Refusal>,
    pub journal: Result<Vec<JournalEntry>, Refusal>,
    pub receipts: Vec<(String, Result<Receipt, Refusal>)>,
    pub trials: Vec<(String, Result<TrialRecord, Refusal>)>,
    pub strays: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub code: Code,
    pub file: String,
    pub detail: String,
}

/// Checks every loaded document against the binding. An unreadable document is a finding and
/// affects nothing else; a readable one written for another identity is `E_IDENTITY_MISMATCH`.
/// Interrupted operations are findings too (`E_OP_INTERRUPTED`), reported, not replayed.
pub fn verify_identity(l: &Loaded, b: &Binding) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut add = |code, file: &str, detail: String| {
        out.push(Finding {
            code,
            file: file.into(),
            detail,
        })
    };
    let stamp = |file: &str, s: &Stamp| {
        (!s.matches(b)).then(|| {
            (
                file.to_string(),
                format!("written for {} as {}", s.profile, s.machine_id),
            )
        })
    };
    let mut mismatches = Vec::new();
    match &l.state {
        Ok(d) => mismatches.extend(stamp("state.json", &d.binding)),
        Err(r) => add(r.code, "state.json", r.detail.clone()),
    }
    match &l.ledger {
        Ok(d) => mismatches.extend(stamp("ledger.json", &d.binding)),
        Err(r) => add(r.code, "ledger.json", r.detail.clone()),
    }
    for (n, r) in &l.receipts {
        match r {
            Ok(r) => mismatches.extend(stamp(&format!("receipts/{n}"), &r.stamp())),
            Err(e) => add(e.code, &format!("receipts/{n}"), e.detail.clone()),
        }
    }
    for (n, t) in &l.trials {
        match t {
            Ok(t) => mismatches.extend(stamp(&format!("trials/{n}"), &t.binding)),
            Err(e) => add(e.code, &format!("trials/{n}"), e.detail.clone()),
        }
    }
    for (f, d) in mismatches {
        add(Code::IdentityMismatch, &f, d);
    }
    match &l.journal {
        Ok(entries) => {
            for e in state::unfinished(entries) {
                add(
                    Code::OpInterrupted,
                    "journal.jsonl",
                    format!(
                        "operation {} of run {} has an intent and no outcome",
                        e.op, e.op_id
                    ),
                );
            }
        }
        Err(r) => add(r.code, "journal.jsonl", r.detail.clone()),
    }
    out
}
