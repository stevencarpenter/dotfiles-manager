//! The state directory: initialization, journal, ledger, receipts, trials and snapshots.
//!
//! A [`Store`] holds open descriptors for the state root and for `state/`. Every read and write
//! goes through them, so a path swapped after the store was opened cannot redirect an
//! operation. Before each write the store also checks that the held directories are still the
//! ones the paths name and that the lock is still held on the lock file.

use crate::fail::Fail;
use crate::fsys::{Dir, Layout, Lock, Sys, below, path_ident, storage, unconfirmed};
use dfm_model::binding::Binding;
use dfm_model::state::{
    self, Class, JournalEntry, Ledger, Phase, Receipt, Scope, Stamp, StateDoc, TrialRecord,
    canonical,
};
use dfm_model::{Code, Refusal};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

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
    /// Canonical HOME as a path, for claim checks (claims live outside the manager's storage).
    home: PathBuf,
    root_parent: Dir,
    root_name: String,
    root: Dir,
    state: Option<Dir>,
    scope: Scope,
    /// Manager paths (state root, config, lock, data, cache) as canonical paths, for identity checks.
    reserved: Vec<PathBuf>,
    /// Set after the first write that took effect. Later refusals are stamped with it.
    mutated: AtomicBool,
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

fn rel_of(layout: &Layout, p: &Path) -> Result<Vec<String>, Refusal> {
    below(&layout.home, p)
}

impl Store {
    /// Opens HOME and walks to the state root, creating directories when `create` is set.
    fn locate(
        sys: &Sys,
        layout: &Layout,
        home: &Dir,
        create: bool,
    ) -> Result<Option<(Dir, String, Dir)>, Refusal> {
        let mut rel = rel_of(layout, &layout.state_root)?;
        let Some(name) = rel.pop() else {
            return Err(storage("the state root is HOME itself"));
        };
        // Shared parents (such as .local/state) only have to be closed to group and others.
        let Some(parent) = home.walk(sys, &rel, rel.len(), create)? else {
            return Ok(None);
        };
        let Some(root) = parent.child(sys, &name, true, create)? else {
            return Ok(None);
        };
        Ok(Some((parent, name, root)))
    }

    fn build(
        sys: Sys,
        layout: &Layout,
        home: &Dir,
        located: (Dir, String, Dir),
        fail: Fail,
    ) -> Result<Store, Refusal> {
        let (root_parent, root_name, root) = located;
        let canon = home.shown().to_path_buf();
        let join = |p: &Path| -> Result<PathBuf, Refusal> {
            Ok(rel_of(layout, p)?
                .iter()
                .fold(canon.clone(), |a, c| a.join(c)))
        };
        let reserved_paths = vec![
            join(&layout.state_root)?,
            join(&layout.config_dir)?,
            join(&layout.lock_file)?,
            join(&layout.data_dir)?,
            join(&layout.cache_dir)?,
        ];
        let reserved_text: Vec<String> = reserved_paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        Ok(Store {
            sys,
            scope: Scope::new(&canon.to_string_lossy(), &reserved_text),
            home: canon,
            reserved: reserved_paths,
            mutated: AtomicBool::new(false),
            root_parent,
            root_name,
            root,
            state: None,
            secrets: Vec::new(),
            fail,
            lock: None,
        })
    }

    /// Opens `state/` and its subdirectories if they exist and validates them on their
    /// descriptors. `false` when `state` is absent.
    fn attach_state(&mut self) -> Result<bool, Refusal> {
        let Some(state) = self.root.child(&self.sys, STATE, true, false)? else {
            self.state = None;
            return Ok(false);
        };
        for d in SUBDIRS {
            state.child(&self.sys, d, true, false)?;
        }
        self.state = Some(state);
        Ok(true)
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
        let home = Dir::open_home(&sys, &layout.home)?;
        let Some(located) = Self::locate(&sys, layout, &home, false)? else {
            return Ok(None);
        };
        let mut s = Store::build(sys, layout, &home, located, fail)?;
        Ok(s.attach_state()?.then_some(s))
    }

    /// Opens for writing and may create the state root: see [`Store::open_write_with`].
    pub fn open_write(sys: Sys, layout: &Layout, fail: Fail) -> Result<Store, Refusal> {
        Self::open_write_with(sys, layout, fail, true)
    }

    /// Opens for writing: takes the exclusive lock (`E_LOCKED` if held, never waiting), then
    /// validates the manager root. With `create_root` the missing parents of the lock file and
    /// the state root are created, which only the first `bind` should do. Without it a missing
    /// directory is state loss and a refusal (`E_STATE_INVALID`) that creates nothing. The lock
    /// file itself is created if absent: it holds no state.
    pub fn open_write_with(
        sys: Sys,
        layout: &Layout,
        fail: Fail,
        create_root: bool,
    ) -> Result<Store, Refusal> {
        let lost = || {
            Refusal::new(
                Code::StateInvalid,
                "the state directory is missing; state is not recreated (state loss)",
            )
        };
        let home = Dir::open_home(&sys, &layout.home)?;
        let mut lock_rel = rel_of(layout, &layout.lock_file)?;
        let Some(lock_name) = lock_rel.pop() else {
            return Err(storage("the lock file is HOME itself"));
        };
        let lock_dir = home
            .walk(&sys, &lock_rel, lock_rel.len(), create_root)?
            .ok_or_else(lost)?;
        let lock = Lock::acquire(&sys, &lock_dir, &lock_name)?;
        let located = Self::locate(&sys, layout, &home, create_root)?.ok_or_else(lost)?;
        let mut s = Store::build(sys, layout, &home, located, fail)?;
        s.lock = Some(lock);
        // A writer must not act on a state directory whose layout it would refuse to read.
        s.attach_state()?;
        Ok(s)
    }

    fn lock_ok(&self) -> Result<(), Refusal> {
        self.lock
            .as_ref()
            .ok_or_else(|| storage("a write needs the manager lock"))?
            .verify()
    }

    /// The held directory still has the owner and mode it was opened with.
    fn held_ok(&self, d: &Dir, private: bool) -> Result<(), Refusal> {
        d.recheck(&self.sys, private)
    }

    /// Run immediately before every write. The lock file is still the locked one. The state root
    /// and `state/` that this store holds are still the entries their paths name (a swap is
    /// refused) and still have the right owner and mode. Returns the subdirectory `sub`, opened
    /// without following a link, when one is asked for.
    fn guard(&self, sub: Option<&str>) -> Result<Option<Dir>, Refusal> {
        self.lock_ok()?;
        let replaced =
            |what: &str| storage(format!("{what} was replaced or removed; this writer stops"));
        if self.root_parent.entry_ident(&self.root_name)? != Some(self.root.ident()?) {
            return Err(replaced("the state root"));
        }
        self.held_ok(&self.root, true)?;
        let state = self.state()?;
        if self.root.entry_ident(STATE)? != Some(state.ident()?) {
            return Err(replaced("the state directory"));
        }
        self.held_ok(state, true)?;
        match sub {
            None => Ok(None),
            Some(s) => state
                .child(&self.sys, s, true, false)?
                .map(Some)
                .ok_or_else(|| storage(format!("{s} is missing; it is not recreated"))),
        }
    }

    fn state(&self) -> Result<&Dir, Refusal> {
        self.state
            .as_ref()
            .ok_or_else(|| storage("the state directory does not exist"))
    }

    /// The state directory exists (and passed topology validation).
    pub fn is_initialized(&self) -> bool {
        self.state.is_some()
    }

    /// Creates the state directory for `stamp`. It builds `state.next` and renames it into place,
    /// so a crash leaves either no `state` or a complete one. An existing `state` that is not an
    /// empty directory is refused and left alone: lost or damaged state is never recreated.
    pub fn init(&mut self, stamp: &Stamp) -> Result<(), Refusal> {
        let r = self.init_inner(stamp);
        self.wrote(r)
    }

    fn init_inner(&mut self, stamp: &Stamp) -> Result<(), Refusal> {
        // The state directory may not exist yet, so only the lock and the root are checked.
        self.lock_ok()?;
        if self.root_parent.entry_ident(&self.root_name)? != Some(self.root.ident()?) {
            return Err(storage(
                "the state root was replaced or removed; init stops",
            ));
        }
        self.held_ok(&self.root, true)?;
        if let Some(state) = &self.state
            && !state.list()?.names.is_empty()
        {
            return Err(Refusal::new(
                Code::StateInvalid,
                format!(
                    "{} exists and is not empty; it is not recreated",
                    state.shown().display()
                ),
            ));
        }
        // A half-built directory from a crashed init never held authority; discard it.
        self.root.remove_tree(&self.sys, STAGING)?;
        let staging = self
            .root
            .child(&self.sys, STAGING, true, true)?
            .ok_or_else(|| storage("cannot create the staging directory"))?;
        for d in SUBDIRS {
            staging.child(&self.sys, d, true, true)?;
        }
        let doc = StateDoc::new(stamp.clone(), now_unix()?);
        staging.publish_new("state.json", &canonical(&doc)?, &self.fail, "init-state")?;
        staging.publish_new(
            "ledger.json",
            &canonical(&Ledger::empty(stamp.clone()))?,
            &self.fail,
            "init-ledger",
        )?;
        staging.publish_new("journal.jsonl", b"", &self.fail, "init-journal")?;
        self.fail.hit("init-before-rename")?;
        // Just before the rename: the lock is still ours and the staging name still names the
        // directory this call built.
        self.lock_ok()?;
        if self.root.entry_ident(STAGING)? != Some(staging.ident()?) {
            return Err(storage("the staging directory was replaced; init stops"));
        }
        self.root.rename(STAGING, STATE)?;
        let synced = unconfirmed(self.root.sync());
        self.attach_state()?;
        synced
    }

    fn read_doc(&self, name: &str) -> Result<Vec<u8>, Refusal> {
        self.state()?
            .read_private(&self.sys, name, state::MAX_BYTES)?
            .ok_or_else(|| {
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
        for o in l.resources.iter().flat_map(|r| &r.owned_outputs) {
            self.check_claim(&o.path)?;
        }
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
            for o in e.recovery.iter().flat_map(|r| &r.outputs) {
                self.check_claim(&o.path)?;
            }
        }
        Ok(entries)
    }

    /// The manager's reserved directories and files, and their ancestors, by filesystem identity
    /// (device and inode), not by spelling. A case or Unicode alias, a hard path through a
    /// symlink, or any other name for the same directory has the same identity.
    fn reserved_idents(
        &self,
    ) -> Result<(Vec<crate::fsys::Ident>, Vec<crate::fsys::Ident>), Refusal> {
        let mut inside = Vec::new();
        let mut exact = Vec::new();
        for (i, r) in self.reserved.iter().enumerate() {
            // Index 2 is the lock file: it may be claimed neither exactly nor from inside.
            let is_dir = i != 2;
            if let Some(id) = path_ident(r, true)? {
                exact.push(id);
                if is_dir {
                    inside.push(id);
                }
            }
            for a in r
                .ancestors()
                .skip(1)
                .take_while(|a| a.starts_with(&self.home))
            {
                if let Some(id) = path_ident(a, true)? {
                    exact.push(id);
                }
            }
        }
        Ok((inside, exact))
    }

    /// Refuses a claim that is, lies inside, or contains a reserved path, judged by identity: a
    /// directory above the leaf (symlinks followed) that is a reserved directory, or a leaf that
    /// is a reserved path or an ancestor of one. A symlink as the leaf is allowed: the link
    /// itself is the claim.
    fn identity_check(&self, path: &str) -> Result<(), Refusal> {
        let outside = || {
            Refusal::new(
                Code::StateInvalid,
                format!("{path} is, contains or lies inside a path the manager owns"),
            )
        };
        let (inside, exact) = self.reserved_idents()?;
        let p = Path::new(path);
        for a in p
            .ancestors()
            .skip(1)
            .take_while(|a| a.starts_with(&self.home))
        {
            if path_ident(a, true)?.is_some_and(|id| inside.contains(&id)) {
                return Err(outside());
            }
        }
        if path_ident(p, false)?.is_some_and(|id| exact.contains(&id)) {
            return Err(outside());
        }
        Ok(())
    }

    /// Delete authority for one exact output, decided now: the ledger is read again (valid,
    /// inside the scope, written for this identity), the path is checked against the manager's
    /// reserved paths by identity, and the live fingerprint must equal the recorded one.
    pub fn authorizes_delete(
        &self,
        resource: &str,
        path: &str,
        live_sha256: &str,
    ) -> Result<bool, Refusal> {
        let ledger = self.read_ledger()?;
        self.check_claim(path)?;
        Ok(ledger.authorizes_delete(resource, path, live_sha256))
    }

    /// Whether this store has changed anything on disk yet.
    pub fn mutated(&self) -> bool {
        self.mutated.load(Ordering::SeqCst)
    }

    /// Wraps a write. The first write that took effect (or whose outcome is unconfirmed) sets the
    /// flag, and every refusal after that is stamped as happening after a mutation (exit 1).
    fn wrote<T>(&self, r: Result<T, Refusal>) -> Result<T, Refusal> {
        match &r {
            Ok(_) => self.mutated.store(true, Ordering::SeqCst),
            Err(e) if e.code == Code::IoUnconfirmed => self.mutated.store(true, Ordering::SeqCst),
            Err(_) => {}
        }
        r.map_err(|e| {
            if self.mutated() {
                e.after_mutation()
            } else {
                e
            }
        })
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
        self.identity_check(path)?;
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
                Err(e) => return Err(crate::fsys::io("resolve claim parent", &e)),
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
        self.wrote(self.append_journal_inner(e))
    }

    fn append_journal_inner(&self, e: &JournalEntry) -> Result<(), Refusal> {
        self.guard(None)?;
        let mut e = e.clone();
        e.scrub(&self.secret_refs())?;
        for o in e.recovery.iter().flat_map(|r| &r.outputs) {
            self.check_claim(&o.path)?;
        }
        self.state()?.append_line(
            &self.sys,
            "journal.jsonl",
            &e.to_line()?,
            state::MAX_JOURNAL_BYTES,
            &self.fail,
            "journal",
        )
    }

    /// Refuses a document over the write cap, before anything is written.
    fn cap(bytes: Vec<u8>) -> Result<Vec<u8>, Refusal> {
        if bytes.len() > state::MAX_WRITE_BYTES {
            return Err(Refusal::new(
                Code::LimitExceeded,
                format!(
                    "the document would be {} bytes; the write cap is {}",
                    bytes.len(),
                    state::MAX_WRITE_BYTES
                ),
            ));
        }
        Ok(bytes)
    }

    /// Replaces the ledger atomically. A failure before the rename leaves the previous ledger. A
    /// failure after it is `E_IO_UNCONFIRMED`: the new ledger is in place, durability unknown.
    /// The ledger must carry the identity in `state.json`.
    pub fn write_ledger(&self, l: &Ledger) -> Result<(), Refusal> {
        self.wrote(self.write_ledger_inner(l))
    }

    fn write_ledger_inner(&self, l: &Ledger) -> Result<(), Refusal> {
        self.guard(None)?;
        self.check_owner(&l.binding)?;
        l.validate()?;
        l.check_secrets(&self.secret_refs())?;
        for o in l.resources.iter().flat_map(|r| &r.owned_outputs) {
            self.check_claim(&o.path)?;
        }
        let bytes = Self::cap(canonical(l)?)?;
        self.state()?
            .replace_file("ledger.json", &bytes, &self.fail, "ledger")
    }

    /// Publishes a receipt. Receipts are immutable: an existing name is a refusal, never an
    /// overwrite. Returns the file name.
    pub fn write_receipt(&self, r: &Receipt) -> Result<String, Refusal> {
        self.wrote(self.write_receipt_inner(r))
    }

    fn write_receipt_inner(&self, r: &Receipt) -> Result<String, Refusal> {
        let dir = self
            .guard(Some("receipts"))?
            .ok_or_else(|| storage("no receipts directory"))?;
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
        if dir.publish_new(&name, &Self::cap(canonical(&r)?)?, &self.fail, "receipt")? {
            Ok(name)
        } else {
            Err(storage(format!(
                "receipt {name} already exists; receipts are immutable"
            )))
        }
    }

    /// Replaces an active trial record atomically.
    pub fn write_trial(&self, t: &TrialRecord) -> Result<(), Refusal> {
        self.wrote(self.write_trial_inner(t))
    }

    fn write_trial_inner(&self, t: &TrialRecord) -> Result<(), Refusal> {
        let dir = self
            .guard(Some("trials"))?
            .ok_or_else(|| storage("no trials directory"))?;
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
        dir.replace_file(
            &format!("{}.json", t.trial_id),
            &Self::cap(canonical(t)?)?,
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
        self.wrote(self.write_snapshot_inner(op_id, name, bytes, class))
    }

    fn write_snapshot_inner(
        &self,
        op_id: &str,
        name: &str,
        bytes: &[u8],
        class: Class,
    ) -> Result<(), Refusal> {
        let snaps = self
            .guard(Some("snapshots"))?
            .ok_or_else(|| storage("no snapshots directory"))?;
        if class == Class::Sensitive {
            return Err(storage("a sensitive resource has no snapshot content"));
        }
        if !state::valid_id(op_id) || !state::valid_id(name) {
            return Err(storage("snapshot names must be plain ids"));
        }
        if bytes.len() > state::MAX_WRITE_BYTES {
            return Err(Refusal::new(
                Code::LimitExceeded,
                "the snapshot is over the write cap",
            ));
        }
        let dir = snaps
            .child(&self.sys, op_id, true, true)?
            .ok_or_else(|| storage("cannot create the snapshot directory"))?;
        if dir.publish_new(name, bytes, &self.fail, "snapshot")? {
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

    /// Lists a directory. A listing error and a name that is not UTF-8 are findings, not
    /// silently skipped.
    fn list_dir(d: &Dir, shown: &str, problems: &mut Vec<Finding>) -> Vec<String> {
        match d.list() {
            Ok(l) => {
                if l.odd > 0 {
                    problems.push(Finding::new(
                        Code::StateInvalid,
                        shown,
                        "an entry name is not UTF-8",
                    ));
                }
                l.names
            }
            Err(e) => {
                problems.push(Finding::new(
                    Code::StateInvalid,
                    shown,
                    format!("cannot list this directory: {}", e.detail),
                ));
                Vec::new()
            }
        }
    }

    /// Opens a subdirectory of `state/` for reading, reporting a failure as a finding.
    fn sub_dir(&self, sub: &str, problems: &mut Vec<Finding>) -> Option<Dir> {
        let opened = self
            .state()
            .and_then(|s| s.child(&self.sys, sub, true, false));
        match opened {
            Ok(Some(d)) => Some(d),
            Ok(None) => {
                problems.push(Finding::new(
                    Code::StateInvalid,
                    sub,
                    "this directory is missing",
                ));
                None
            }
            Err(e) => {
                problems.push(Finding::new(
                    Code::StateInvalid,
                    sub,
                    format!("cannot list this directory: {}", e.detail),
                ));
                None
            }
        }
    }

    /// Reads every document. Each file's result is kept separately so one unreadable document
    /// neither hides nor changes the others.
    pub fn load(&self) -> Loaded {
        let mut problems = Vec::new();
        let mut strays = Vec::new();
        let tmp = |n: &str| n.starts_with('.') && n.ends_with(".tmp");
        let read = |d: &Dir, n: &str| -> Result<Vec<u8>, Refusal> {
            d.read_private(&self.sys, n, state::MAX_BYTES)?
                .ok_or_else(|| storage("vanished"))
        };
        let mut receipts = Vec::new();
        if let Some(d) = self.sub_dir("receipts", &mut problems) {
            for n in Self::list_dir(&d, "receipts", &mut problems) {
                if tmp(&n) {
                    strays.push(format!("receipts/{n} (interrupted write)"));
                    continue;
                }
                let r = read(&d, &n).and_then(|b| Receipt::parse(&b)).and_then(|r| {
                    r.validate_within(&self.scope)?;
                    for p in r.operations.iter().flat_map(|o| &o.owned_outputs) {
                        self.check_claim(p)?;
                    }
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
        }
        let mut trials = Vec::new();
        if let Some(d) = self.sub_dir("trials", &mut problems) {
            for n in Self::list_dir(&d, "trials", &mut problems) {
                if tmp(&n) {
                    strays.push(format!("trials/{n} (interrupted write)"));
                    continue;
                }
                let t = read(&d, &n)
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
        }
        if let Some(d) = self.sub_dir("snapshots", &mut problems) {
            for op in Self::list_dir(&d, "snapshots", &mut problems) {
                let op_dir = format!("snapshots/{op}");
                match d.child(&self.sys, &op, true, false) {
                    Ok(Some(sd)) => {
                        for n in Self::list_dir(&sd, &op_dir, &mut problems) {
                            if tmp(&n) {
                                strays.push(format!("{op_dir}/{n} (interrupted write)"));
                            }
                        }
                    }
                    _ => strays.push(format!("{op_dir} (unexpected)")),
                }
            }
        }
        if let Ok(state) = self.state() {
            for n in Self::list_dir(state, STATE, &mut problems) {
                let known = ["state.json", "ledger.json", "journal.jsonl"];
                if !known.contains(&n.as_str()) && !SUBDIRS.contains(&n.as_str()) {
                    strays.push(format!("{n} (unexpected)"));
                }
            }
        }
        for n in Self::list_dir(&self.root, "..", &mut problems) {
            if n != STATE {
                strays.push(format!("../{n} (staging or previous state)"));
            }
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
