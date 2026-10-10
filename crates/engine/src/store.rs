//! The state directory: initialization, journal, ledger, receipts, trials and snapshots.

use crate::fail::Fail;
use crate::fsys::{
    self, Found, Layout, Lock, Sys, append_line, below, canonical_home, check_dir_chain,
    publish_new, read_private, replace_file, storage, unconfirmed,
};
use dfm_model::binding::Binding;
use dfm_model::state::{
    self, Class, JournalEntry, Ledger, Phase, Receipt, Scope, Stamp, StateDoc, TrialRecord,
    canonical,
};
use dfm_model::{Code, Refusal};
use std::fs;
use std::path::{Path, PathBuf};

/// Seconds since the epoch. A clock before 1970 is an error, never a silent zero.
pub fn now_unix() -> Result<u64, Refusal> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| Refusal::new(Code::IoFailed, "the system clock is before 1970"))
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
    root_rel: Vec<String>,
    dir: PathBuf,
    scope: Scope,
    secrets: Vec<String>,
    fail: Fail,
    lock: Option<Lock>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub code: Code,
    pub file: String,
    pub detail: String,
}

impl Finding {
    fn new(code: Code, file: impl Into<String>, detail: impl Into<String>) -> Finding {
        Finding {
            code,
            file: file.into(),
            detail: detail.into(),
        }
    }
}

impl Store {
    fn new(sys: Sys, layout: &Layout, home: PathBuf, fail: Fail) -> Result<Store, Refusal> {
        let join = |p: &Path| -> Result<PathBuf, Refusal> {
            Ok(below(&layout.home, p)?
                .iter()
                .fold(home.clone(), |a, c| a.join(c)))
        };
        let root_rel = below(&layout.home, &layout.state_root)?;
        if root_rel.is_empty() {
            return Err(storage("the state root is HOME itself"));
        }
        let root = join(&layout.state_root)?;
        let reserved: Vec<String> = [
            root.clone(),
            join(&layout.config_dir)?,
            join(&layout.lock_file)?,
            join(&layout.data_dir)?,
            join(&layout.cache_dir)?,
        ]
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
        let home_str = home.to_string_lossy().into_owned();
        Ok(Store {
            sys,
            dir: root.join(STATE),
            scope: Scope::new(&home_str, &reserved),
            secrets: Vec::new(),
            root_rel,
            root,
            home,
            fail,
            lock: None,
        })
    }

    /// Known secret values. Descriptive text that contains one is redacted before it is
    /// persisted, and a path or identifier that contains one is refused.
    pub fn with_secrets(mut self, secrets: Vec<String>) -> Store {
        self.secrets = secrets;
        self
    }

    fn secret_refs(&self) -> Vec<&str> {
        self.secrets.iter().map(String::as_str).collect()
    }

    /// Opens for reading without taking the lock and without creating anything. `None` when no
    /// state directory exists. A redirected, mistyped or badly owned path is a refusal.
    pub fn open_read(sys: Sys, layout: &Layout, fail: Fail) -> Result<Option<Store>, Refusal> {
        let home = canonical_home(&sys, &layout.home)?;
        let s = Store::new(sys, layout, home.clone(), fail)?;
        let private_from = s.root_rel.len() - 1;
        if check_dir_chain(&sys, &home, &s.root_rel, private_from, false)? == Found::Absent {
            return Ok(None);
        }
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
        let mut s = Store::new(sys, layout, home.clone(), fail)?;
        check_dir_chain(&sys, &home, &s.root_rel, s.root_rel.len() - 1, true)?;
        s.lock = Some(lock);
        // A writer must not act on a state directory whose layout it would refuse to read.
        s.check_layout()?;
        Ok(s)
    }

    /// Run immediately before every write: the lock file is still the locked one, and the
    /// whole directory chain down to `sub` (a subdirectory of `state`, or none) still passes.
    /// This narrows the window between validation and use. It does not close it: see the
    /// module note in `fsys.rs`.
    fn guard(&self, sub: Option<&str>) -> Result<(), Refusal> {
        self.lock
            .as_ref()
            .ok_or_else(|| storage("a write needs the manager lock"))?
            .verify()?;
        let gone = |what: &str| storage(format!("{what} vanished before the write"));
        let private_from = self.root_rel.len() - 1;
        if check_dir_chain(&self.sys, &self.home, &self.root_rel, private_from, false)?
            == Found::Absent
        {
            return Err(gone("the state root"));
        }
        let mut below_root = vec![STATE.to_string()];
        below_root.extend(sub.map(str::to_string));
        if check_dir_chain(&self.sys, &self.root, &below_root, 0, false)? == Found::Absent {
            return Err(gone("the state directory"));
        }
        Ok(())
    }

    /// The state directory exists (and passed topology validation).
    pub fn is_initialized(&self) -> Result<bool, Refusal> {
        Ok(check_dir_chain(&self.sys, &self.root, &[STATE.into()], 0, false)? == Found::Present)
    }

    /// Creates the state directory for `stamp`. It builds `state.next` and renames it into place,
    /// so a crash leaves either no `state` or a complete one. An existing `state` that is not an
    /// empty directory is refused and left alone: lost or damaged state is never recreated.
    pub fn init(&self, stamp: &Stamp) -> Result<(), Refusal> {
        // The state directory does not exist yet, so only the lock and the root are checked.
        self.lock_ok()?;
        let private_from = self.root_rel.len() - 1;
        if check_dir_chain(&self.sys, &self.home, &self.root_rel, private_from, false)?
            == Found::Absent
        {
            return Err(storage("the state root vanished before init"));
        }
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
        let doc = StateDoc::new(stamp.clone(), now_unix()?);
        publish_new(
            &staging,
            "state.json",
            &canonical(&doc)?,
            &self.fail,
            "init-state",
        )?;
        publish_new(
            &staging,
            "ledger.json",
            &canonical(&Ledger::empty(stamp.clone()))?,
            &self.fail,
            "init-ledger",
        )?;
        publish_new(&staging, "journal.jsonl", b"", &self.fail, "init-journal")?;
        self.fail.hit("init-before-rename")?;
        fs::rename(&staging, dir).map_err(|e| fsys::io("publish state", &e))?;
        unconfirmed(fsys::sync_dir(&self.root))
    }

    fn lock_ok(&self) -> Result<(), Refusal> {
        self.lock
            .as_ref()
            .ok_or_else(|| storage("a write needs the manager lock"))?
            .verify()
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
            Err(mismatch(&doc.binding, b.profile(), b.machine_id()))
        }
    }

    /// A document must carry the identity recorded in `state.json`.
    fn check_owner(&self, s: &Stamp) -> Result<(), Refusal> {
        let doc = self.read_state()?;
        if doc.binding.same_identity(s) {
            Ok(())
        } else {
            Err(mismatch(&doc.binding, &s.profile, &s.machine_id))
        }
    }

    fn ledger_raw(&self) -> Result<Ledger, Refusal> {
        let l = Ledger::parse(&self.read_doc("ledger.json")?)?;
        l.validate_within(&self.scope)?;
        Ok(l)
    }

    /// The ledger, valid and inside the scope, and written for the identity in `state.json`.
    pub fn read_ledger(&self) -> Result<Ledger, Refusal> {
        let l = self.ledger_raw()?;
        self.check_owner(&l.binding)?;
        Ok(l)
    }

    pub fn read_journal(&self) -> Result<Vec<JournalEntry>, Refusal> {
        let entries = state::parse_journal(&self.read_doc("journal.jsonl")?)?;
        for e in &entries {
            e.validate_within(&self.scope)?;
        }
        Ok(entries)
    }

    /// A claim must lie inside HOME and outside the manager's own paths, as written and after the
    /// symlinks in its parent directories are resolved. Whether a path in HOME belongs to the
    /// manager is the adapter's evidence, not a guess.
    fn check_claim(&self, path: &str) -> Result<(), Refusal> {
        let outside = || {
            Refusal::new(
                Code::StateInvalid,
                format!("{path} is outside HOME or overlaps the manager's own paths"),
            )
        };
        if !self.scope.admits(path) {
            return Err(outside());
        }
        let p = Path::new(path);
        let (Some(parent), Some(leaf)) = (p.parent(), p.file_name()) else {
            return Err(outside());
        };
        // Resolve the deepest existing ancestor of the parent, keep the rest as written.
        let mut base = parent.to_path_buf();
        let mut tail = Vec::new();
        let real = loop {
            match fs::canonicalize(&base) {
                Ok(r) => break r,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let Some(name) = base.file_name().map(ToOwned::to_owned) else {
                        return Err(outside());
                    };
                    tail.push(name);
                    if !base.pop() {
                        return Err(outside());
                    }
                }
                Err(e) => return Err(fsys::io("resolve claim parent", &e)),
            }
        };
        let mut full = real;
        full.extend(tail.iter().rev());
        full.push(leaf);
        match full.to_str() {
            Some(s) if self.scope.admits(s) => Ok(()),
            _ => Err(outside()),
        }
    }

    /// Appends one journal entry and syncs it. Intent entries are written before a side effect.
    pub fn append_journal(&self, e: &JournalEntry) -> Result<(), Refusal> {
        self.guard(None)?;
        let mut e = e.clone();
        e.scrub(&self.secret_refs())?;
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

    /// Replaces the ledger atomically. A failure before the rename leaves the previous ledger. A
    /// failure after it is `E_IO_UNCONFIRMED`: the new ledger is in place, durability unknown.
    /// The ledger must carry the identity in `state.json`.
    pub fn write_ledger(&self, l: &Ledger) -> Result<(), Refusal> {
        self.guard(None)?;
        self.check_owner(&l.binding)?;
        l.validate()?;
        l.check_secrets(&self.secret_refs())?;
        for o in l.resources.iter().flat_map(|r| &r.owned_outputs) {
            self.check_claim(&o.path)?;
        }
        replace_file(
            &self.dir,
            "ledger.json",
            &canonical(l)?,
            &self.fail,
            "ledger",
        )
    }

    /// Publishes a receipt. Receipts are immutable: an existing name is a refusal, never an
    /// overwrite. Returns the file name.
    pub fn write_receipt(&self, r: &Receipt) -> Result<String, Refusal> {
        self.guard(Some("receipts"))?;
        self.check_owner(&r.stamp())?;
        let mut r = r.clone();
        r.scrub(&self.secret_refs())?;
        r.validate()?;
        for p in r.operations.iter().flat_map(|o| &o.owned_outputs) {
            self.check_claim(p)?;
        }
        let name = r
            .file_name()
            .ok_or_else(|| Refusal::new(Code::StateInvalid, "receipt has no usable time"))?;
        if publish_new(
            &self.dir.join("receipts"),
            &name,
            &canonical(&r)?,
            &self.fail,
            "receipt",
        )? {
            Ok(name)
        } else {
            Err(storage(format!(
                "receipt {name} already exists; receipts are immutable"
            )))
        }
    }

    /// Replaces an active trial record atomically.
    pub fn write_trial(&self, t: &TrialRecord) -> Result<(), Refusal> {
        self.guard(Some("trials"))?;
        self.check_owner(&t.binding)?;
        t.validate()?;
        t.check_secrets(&self.secret_refs())?;
        for p in t
            .owned_outputs
            .iter()
            .chain(t.baseline.paths.iter().map(|b| &b.path))
        {
            self.check_claim(p)?;
        }
        replace_file(
            &self.dir.join("trials"),
            &format!("{}.json", t.trial_id),
            &canonical(t)?,
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
        self.guard(Some("snapshots"))?;
        if class == Class::Sensitive {
            return Err(storage("a sensitive resource has no snapshot content"));
        }
        if !state::valid_id(op_id) || !state::valid_id(name) {
            return Err(storage("snapshot names must be plain ids"));
        }
        check_dir_chain(
            &self.sys,
            &self.dir.join("snapshots"),
            &[op_id.to_string()],
            0,
            true,
        )?;
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
        self.guard(None)?;
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
                at: state::rfc3339(now_unix()?),
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

    /// Lists a directory below `state`. A listing error, a missing directory and a name that is
    /// not UTF-8 are findings, not silently skipped.
    fn list(&self, rel: &str, problems: &mut Vec<Finding>) -> Vec<String> {
        let shown = if rel.is_empty() { STATE } else { rel };
        let rd = match fs::read_dir(self.dir.join(rel)) {
            Ok(rd) => rd,
            Err(e) => {
                problems.push(Finding::new(
                    Code::StateInvalid,
                    shown,
                    format!("cannot list this directory: {}", e.kind()),
                ));
                return Vec::new();
            }
        };
        let mut names = Vec::new();
        for entry in rd {
            match entry.map(|e| e.file_name().into_string()) {
                Ok(Ok(n)) => names.push(n),
                Ok(Err(_)) => problems.push(Finding::new(
                    Code::StateInvalid,
                    shown,
                    "an entry name is not UTF-8",
                )),
                Err(e) => problems.push(Finding::new(
                    Code::StateInvalid,
                    shown,
                    format!("cannot read an entry: {}", e.kind()),
                )),
            }
        }
        names.sort();
        names
    }

    /// Reads every document. Each file's result is kept separately so one unreadable document
    /// neither hides nor changes the others.
    pub fn load(&self) -> Loaded {
        let mut problems = Vec::new();
        let mut strays = Vec::new();
        let tmp = |n: &str| n.starts_with('.') && n.ends_with(".tmp");
        let mut receipts = Vec::new();
        for n in self.list("receipts", &mut problems) {
            if tmp(&n) {
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
                r.validate_within(&self.scope)?;
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
        for n in self.list("trials", &mut problems) {
            if tmp(&n) {
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
                t.validate_within(&self.scope)?;
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
        for op in self.list("snapshots", &mut problems) {
            let op_dir = format!("snapshots/{op}");
            let is_dir = fs::symlink_metadata(self.dir.join(&op_dir)).is_ok_and(|m| m.is_dir());
            if !is_dir {
                strays.push(format!("{op_dir} (unexpected)"));
                continue;
            }
            for n in self.list(&op_dir, &mut problems) {
                if tmp(&n) {
                    strays.push(format!("{op_dir}/{n} (interrupted write)"));
                }
            }
        }
        for n in self.list("", &mut problems) {
            let known = ["state.json", "ledger.json", "journal.jsonl"];
            if !known.contains(&n.as_str()) && !SUBDIRS.contains(&n.as_str()) {
                strays.push(format!("{n} (unexpected)"));
            }
        }
        match fs::read_dir(&self.root) {
            Ok(rd) => {
                for e in rd.flatten() {
                    if let Ok(n) = e.file_name().into_string()
                        && n != STATE
                    {
                        strays.push(format!("../{n} (staging or previous state)"));
                    }
                }
            }
            Err(e) => problems.push(Finding::new(
                Code::StateInvalid,
                "..",
                format!("cannot list the state root: {}", e.kind()),
            )),
        }
        strays.sort();
        Loaded {
            state: self.read_state(),
            ledger: self.ledger_raw(),
            journal: self.read_journal(),
            receipts,
            trials,
            strays,
            problems,
        }
    }
}

fn mismatch(owner: &Stamp, profile: &str, machine_id: &str) -> Refusal {
    Refusal::new(
        Code::IdentityMismatch,
        format!(
            "state belongs to {} as {}, not {profile} as {machine_id}",
            owner.profile, owner.machine_id
        ),
    )
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
    /// Directory-level problems found while loading (listing errors, bad names).
    pub problems: Vec<Finding>,
}

/// Checks every loaded document against the binding. An unreadable document is a finding and
/// affects nothing else; a readable one written for another identity is `E_IDENTITY_MISMATCH`.
/// Interrupted operations are findings too (`E_OP_INTERRUPTED`), reported, not replayed.
pub fn verify_identity(l: &Loaded, b: &Binding) -> Vec<Finding> {
    let mut out: Vec<Finding> = l.problems.clone();
    let mut add = |code, file: &str, detail: String| out.push(Finding::new(code, file, detail));
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
