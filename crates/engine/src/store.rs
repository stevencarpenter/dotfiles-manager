//! The state directory: initialization, journal, ledger, receipts, trials and snapshots.
//!
//! A [`Store`] holds open descriptors for the state root and for `state/`. Every read and write
//! goes through them, so a path swapped after the store was opened cannot redirect an
//! operation. Before each write the store also checks that the held directories are still the
//! ones the paths name and that the lock is still held on the lock file.

use crate::fail::Fail;
use crate::fsys::{
    Dir, Layout, Lock, Sys, below, created, path_ident, reset_created, storage, unconfirmed,
};
use dfm_model::binding::Binding;
use dfm_model::lock::Grants;
use dfm_model::state::{
    self, Class, JournalEntry, Ledger, Phase, Receipt, Scope, Stamp, StateDoc, TrialRecord,
    canonical,
};
use dfm_model::{Code, Refusal};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Nanoseconds since the epoch. A clock before 1970 or after 2554 is an error, never a silent
/// zero.
pub fn now_ns() -> Result<u64, Refusal> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_nanos()).ok())
        .ok_or_else(|| {
            Refusal::new(
                Code::IoFailed,
                "the system clock is before 1970 or after 2554",
            )
        })
}

const STATE: &str = "state";
const STAGING: &str = "state.next";
const SUBDIRS: [&str; 3] = ["receipts", "snapshots", "trials"];
/// The per-machine salt for the salted hash of a co-owned file's managed fields.
const SALT: &str = "salt";
/// The origin grants (docs/architecture.md section 8, "Trust grant"; docs/sync.md).
const GRANTS: &str = "grants.json";
const SALT_BYTES: usize = 32;
/// Characters of a cut journal line echoed in the error when its repair record cannot be written.
const REPAIR_ECHO_CHARS: usize = 512;
/// Holds the cut journal text while the repair is unfinished. `state check` reports it as a
/// stray entry, so a repair whose record could not be written stays visible.
const TORN: &str = "journal.torn";

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
    reserved: Vec<Reserved>,
    /// Set after the first write that took effect. Later refusals are stamped with it.
    mutated: AtomicBool,
    secrets: Vec<String>,
    fail: Fail,
    lock: Option<Lock>,
}

/// Filesystem identities of the manager's reserved paths. `inside`: reserved directories (a
/// claim may not lie in one). `own`: the reserved paths themselves. `exact`: `own` plus the
/// ancestors that exist today.
struct ReservedIdents {
    inside: Vec<crate::fsys::Ident>,
    exact: Vec<crate::fsys::Ident>,
    own: Vec<crate::fsys::Ident>,
}

/// A path the manager owns. A claim may not be, lie inside, or contain it.
#[derive(Debug)]
struct Reserved {
    path: PathBuf,
    /// A directory (claims inside it are refused) or the lock file (a plain file).
    is_dir: bool,
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
    /// Opens HOME and walks to the state root, creating directories when `create` is set.
    fn locate(
        sys: &Sys,
        layout: &Layout,
        home: &Dir,
        create: bool,
    ) -> Result<Option<(Dir, String, Dir)>, Refusal> {
        let mut rel = below(&layout.home, &layout.state_root)?;
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
            Ok(below(&layout.home, p)?
                .iter()
                .fold(canon.clone(), |a, c| a.join(c)))
        };
        let reserved_paths = vec![
            Reserved {
                path: join(&layout.state_root)?,
                is_dir: true,
            },
            Reserved {
                path: join(&layout.config_dir)?,
                is_dir: true,
            },
            Reserved {
                path: join(&layout.lock_file)?,
                is_dir: false,
            },
            Reserved {
                path: join(&layout.data_dir)?,
                is_dir: true,
            },
            Reserved {
                path: join(&layout.cache_dir)?,
                is_dir: true,
            },
        ];
        let reserved_text: Vec<String> = reserved_paths
            .iter()
            .map(|r| r.path.to_string_lossy().into_owned())
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
    /// descriptors. `false` when `state` is absent. With `whole`, a missing subdirectory is a
    /// refusal (a writer must not act on a layout `state check` reports as damaged); without
    /// it a reader accepts that layout and `load` reports the missing directory. With `empty_ok`
    /// a `state/` that holds nothing is accepted too: `init` replaces it.
    fn attach_state(&mut self, whole: bool, empty_ok: bool) -> Result<bool, Refusal> {
        let Some(state) = self.root.child(&self.sys, STATE, true, false)? else {
            self.state = None;
            return Ok(false);
        };
        let skip = empty_ok && state.list()?.names.is_empty();
        for d in SUBDIRS {
            if state.child(&self.sys, d, true, false)?.is_none() && whole && !skip {
                return Err(storage(format!(
                    "state/{d} is missing; it is not recreated (state loss)"
                )));
            }
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
        Ok(s.attach_state(false, false)?.then_some(s))
    }

    /// `E_LOCKED` when a writer holds the manager lock. Creates nothing and does not wait. The
    /// returned shared lock (none when no lock file exists) stays held until it is dropped.
    pub fn probe_writer(sys: &Sys, layout: &Layout) -> Result<Option<std::fs::File>, Refusal> {
        let home = Dir::open_home(sys, &layout.home)?;
        let mut rel = below(&layout.home, &layout.lock_file)?;
        let Some(name) = rel.pop() else {
            return Err(storage("the lock file is HOME itself"));
        };
        match home.walk(sys, &rel, rel.len(), false)? {
            Some(dir) => Lock::probe(sys, &dir, &name),
            None => Ok(None),
        }
    }

    /// `E_LOCKED` unless the lock file still is the file `guard` (from [`Store::probe_writer`])
    /// holds. A reader calls it after its read, so a lock file replaced meanwhile is not taken
    /// for a held lock.
    pub fn recheck_probe(sys: &Sys, layout: &Layout, guard: &std::fs::File) -> Result<(), Refusal> {
        let home = Dir::open_home(sys, &layout.home)?;
        let mut rel = below(&layout.home, &layout.lock_file)?;
        let Some(name) = rel.pop() else {
            return Err(storage("the lock file is HOME itself"));
        };
        match home.walk(sys, &rel, rel.len(), false)? {
            Some(dir) => Lock::still_current(&dir, &name, guard),
            None => Err(Refusal::new(
                Code::Locked,
                "the lock directory was removed while it was held; run the command again",
            )),
        }
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
        reset_created();
        match Self::open_write_inner(sys, layout, fail, create_root) {
            Ok(s) => {
                // A directory or the lock file made by this call counts as a mutation.
                s.mutated.store(created(), Ordering::SeqCst);
                Ok(s)
            }
            Err(e) if created() => Err(e.after_mutation()),
            Err(e) => Err(e),
        }
    }

    fn open_write_inner(
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
        let mut lock_rel = below(&layout.home, &layout.lock_file)?;
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
        // A writer must not act on a state directory whose layout `state check` refuses. Only a
        // bind (which runs `init`) may meet an empty one: `init` accepts and replaces it.
        s.attach_state(true, create_root)?;
        Ok(s)
    }

    fn lock_ok(&self) -> Result<(), Refusal> {
        self.lock
            .as_ref()
            .ok_or_else(|| storage("a write needs the manager lock"))?
            .verify()
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
        self.root.recheck(&self.sys, true)?;
        let state = self.state()?;
        if self.root.entry_ident(STATE)? != Some(state.ident()?) {
            return Err(replaced("the state directory"));
        }
        state.recheck(&self.sys, true)?;
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
        self.root.recheck(&self.sys, true)?;
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
        // The staging directory exists from here: a failure now still changed the disk.
        self.mutated.store(true, Ordering::SeqCst);
        for d in SUBDIRS {
            staging.child(&self.sys, d, true, true)?;
        }
        let doc = StateDoc::new(stamp.clone(), now_ns()?);
        staging.publish_new("state.json", &canonical(&doc)?, &self.fail, "init-state")?;
        staging.publish_new(
            "ledger.json",
            &canonical(&Ledger::empty(stamp.clone()))?,
            &self.fail,
            "init-ledger",
        )?;
        staging.publish_new("journal.jsonl", b"", &self.fail, "init-journal")?;
        // The salt exists from the start, so a plan can salt a content token before any merge.
        // A store made by an older build gets it from `bind` or from the first merge.
        staging.publish_new(SALT, &random_salt()?, &self.fail, "init-salt")?;
        self.fail.hit("init-before-rename")?;
        // Just before the rename: the lock is still ours and the staging name still names the
        // directory this call built.
        self.lock_ok()?;
        if self.root.entry_ident(STAGING)? != Some(staging.ident()?) {
            return Err(storage("the staging directory was replaced; init stops"));
        }
        self.root.rename(STAGING, STATE)?;
        let synced = unconfirmed(self.root.sync());
        self.attach_state(true, false)?;
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

    /// The ledger parsed and inside the scope, with no claim check of its paths.
    fn ledger_parsed(&self) -> Result<Ledger, Refusal> {
        let l = Ledger::parse(&self.read_doc("ledger.json")?)?;
        l.validate_within(&self.scope)?;
        Ok(l)
    }

    fn ledger_raw(&self) -> Result<Ledger, Refusal> {
        let l = self.ledger_parsed()?;
        for o in l.resources.iter().flat_map(|r| &r.owned_outputs) {
            self.check_claim(&o.path)?;
        }
        Ok(l)
    }

    /// The ledger, valid and inside the scope, and written for the identity in `state.json`.
    /// One entry that fails the claim check (for example a parent directory that is a link
    /// leaving HOME) refuses the whole read. Writers use this form.
    pub fn read_ledger(&self) -> Result<Ledger, Refusal> {
        let l = self.ledger_raw()?;
        self.check_owner(&l.binding)?;
        Ok(l)
    }

    /// As [`Store::read_ledger`], without the claim check of each output. A path in an old
    /// entry can become unusable later (a parent that became a link leaving HOME, or a loop),
    /// and that must not stop every later run. The caller must not read, write or delete
    /// through an output without checking its own path first: the planner does that per entry
    /// and apply does it per operation. The planner and apply use this form.
    pub fn read_ledger_lenient(&self) -> Result<Ledger, Refusal> {
        let l = self.ledger_parsed()?;
        self.check_owner(&l.binding)?;
        Ok(l)
    }

    /// The per-machine salt for conflict detection on co-owned files (docs/architecture.md
    /// section 8 item 7), or `None` when `state/salt` does not exist yet. It must be exactly 32
    /// bytes in a private regular file. The salt never leaves `state` and is never printed.
    pub fn salt(&self) -> Result<Option<Vec<u8>>, Refusal> {
        // A symlink, a wrong owner or mode, or a wrong size is an error that is never repaired.
        let bad = |e: Refusal| {
            let next = match self.records_salted_hash() {
                Ok(true) => "The ledger records a salted hash: also restore a good salt from a copy, or delete the `coowned` object of each co-owned output in state/ledger.json (the next apply then needs --allow-adopt)".to_string(),
                Ok(false) => "The ledger records no salted hash, so the next apply creates a new salt".to_string(),
                // An unreadable ledger proves nothing about the hash. A new salt would make a
                // recorded hash useless, so do not advise it.
                Err(le) => format!(
                    "The ledger cannot be read ({}), so whether it records a salted hash is unknown. Fix the ledger first; do not replace the salt until it is known",
                    le.detail
                ),
            };
            // The reason without the path: `mode is 644, expected 0600`.
            let why = e.detail.rsplit(": ").next().unwrap_or(&e.detail);
            Refusal::new(
                Code::StorageInvalid,
                format!(
                    "state/salt is unusable ({why}). Fix: move it out of state/ (mv state/salt ~/salt.bad). {next}"
                ),
            )
        };
        match self
            .state()?
            .read_private(&self.sys, SALT, SALT_BYTES + 1)
            .map_err(bad)?
        {
            None => Ok(None),
            Some(b) if b.len() == SALT_BYTES => Ok(Some(b)),
            Some(b) => Err(bad(storage(format!(
                "{} bytes, expected {SALT_BYTES}",
                b.len()
            )))),
        }
    }

    /// Whether the ledger records a salted hash. A ledger that cannot be read is an error, not
    /// "no". The claim check of each output is skipped (as in [`Store::read_ledger_lenient`]):
    /// one output whose parent became an unsafe link does not change what the ledger records.
    fn records_salted_hash(&self) -> Result<bool, Refusal> {
        Ok(self
            .ledger_parsed()?
            .resources
            .iter()
            .flat_map(|r| &r.owned_outputs)
            .any(|o| o.coowned.is_some()))
    }

    /// As [`Store::salt`], and creates it once when absent: 32 random bytes from the operating
    /// system, mode 0600, published with an exclusive rename so a racing writer cannot replace it. Needs the
    /// writer lock.
    pub fn salt_or_create(&self) -> Result<Vec<u8>, Refusal> {
        if let Some(s) = self.salt()? {
            return Ok(s);
        }
        self.wrote(self.create_salt())?;
        self.salt()?
            .ok_or_else(|| storage("state/salt vanished after it was created"))
    }

    fn create_salt(&self) -> Result<(), Refusal> {
        let dir = self.state()?;
        self.guard(None)?;
        dir.publish_new(SALT, &random_salt()?, &self.fail, "salt")?;
        Ok(())
    }

    /// The size of `journal.jsonl` in bytes, as stored.
    pub fn journal_len(&self) -> Result<u64, Refusal> {
        Ok(u64::try_from(self.read_doc("journal.jsonl")?.len()).unwrap_or(u64::MAX))
    }

    /// An upper bound on the entries that settling earlier work will append: one for each open
    /// intent, one for each open `unknown` entry that has an intent record, and one for the repair
    /// of a torn final line. An `unknown` entry with no intent record is never closed, so it
    /// appends nothing and reserves nothing. An entry whose probe stays unclear is still counted,
    /// because the probe result is not known before the run.
    /// A torn final line is skipped here, not an error, because the repair cuts it first.
    pub fn settlement_entries(&self) -> Result<usize, Refusal> {
        let bytes = self.read_doc("journal.jsonl")?;
        let keep = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        let entries = state::parse_journal(&bytes[..keep])?;
        Ok(state::unfinished(&entries).len()
            + state::unresolved(&entries)
                .iter()
                .filter(|u| u.intent.is_some())
                .count()
            + usize::from(keep < bytes.len()))
    }

    pub fn read_journal(&self) -> Result<Vec<JournalEntry>, Refusal> {
        let entries = self.read_journal_lenient()?;
        for e in &entries {
            for o in e.recovery.iter().flat_map(|r| &r.outputs) {
                self.check_entry_path(e, &o.path)?;
            }
        }
        Ok(entries)
    }

    /// As [`Store::read_journal`], without the claim check of each recovery path. A path in an
    /// old entry can become unsafe later (a parent turned into a link that leaves HOME), and
    /// that must not stop every later run. Nothing is read, written or deleted through a path
    /// from these entries without a fresh check. Apply uses this form.
    pub fn read_journal_lenient(&self) -> Result<Vec<JournalEntry>, Refusal> {
        let entries = state::parse_journal(&self.read_doc("journal.jsonl")?)?;
        for e in &entries {
            e.validate_within(&self.scope)?;
        }
        Ok(entries)
    }

    /// The manager's reserved directories and files, and their ancestors, by filesystem identity
    /// (device and inode), not by spelling. A case or Unicode alias, a hard path through a
    /// symlink, or any other name for the same directory has the same identity.
    fn reserved_idents(&self) -> Result<ReservedIdents, Refusal> {
        let mut inside = Vec::new();
        let mut exact = Vec::new();
        let mut own = Vec::new();
        for r in &self.reserved {
            if let Some(id) = path_ident(&r.path, true)? {
                exact.push(id);
                own.push(id);
                // A claim may not lie inside a reserved directory. The lock file has no inside.
                if r.is_dir {
                    inside.push(id);
                }
            }
            // The ancestors that exist today. For a reserved path that is missing this is its
            // deepest existing ancestor. The text scope (folded for case and Unicode) covers the
            // part that does not exist yet.
            for a in r
                .path
                .ancestors()
                .skip(1)
                .take_while(|a| a.starts_with(&self.home))
            {
                if let Some(id) = path_ident(a, true)? {
                    exact.push(id);
                }
            }
        }
        Ok(ReservedIdents { inside, exact, own })
    }

    /// Refuses a claim that is, lies inside, or contains a reserved path, judged by identity: a
    /// directory above the leaf (symlinks followed) that is a reserved directory, or a leaf that
    /// is a reserved path or an ancestor of one. A symlink as the leaf is allowed: the link
    /// itself is the claim.
    ///
    /// `new_dir` is a directory the manager creates while it is missing. It may be an ancestor
    /// of a reserved path, so only the reserved paths themselves count as leaf identities.
    fn identity_check(&self, path: &str, new_dir: bool) -> Result<(), Refusal> {
        let outside = || {
            Refusal::new(
                Code::StateInvalid,
                format!("{path} is, contains or lies inside a path the manager owns"),
            )
        };
        let ReservedIdents { inside, exact, own } = self.reserved_idents()?;
        let leaf_ids = if new_dir { &own } else { &exact };
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
        if path_ident(p, false)?.is_some_and(|id| leaf_ids.contains(&id)) {
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
        // Other entries may be unsafe; only this exact path has to pass the claim check.
        let ledger = self.read_ledger_lenient()?;
        self.check_claim(path)?;
        Ok(ledger.authorizes_delete(resource, path, live_sha256))
    }

    /// Whether this store has changed anything on disk yet.
    pub fn mutated(&self) -> bool {
        self.mutated.load(Ordering::SeqCst)
    }

    /// Records a mutation outside the state files, such as removing a staged output.
    pub(crate) fn mark_mutated(&self) {
        self.mutated.store(true, Ordering::SeqCst);
    }

    /// Wraps a write. The first write that took effect (or whose outcome is unconfirmed) sets the
    /// flag, and every refusal after that is stamped as happening after a mutation (exit 1).
    fn wrote<T>(&self, r: Result<T, Refusal>) -> Result<T, Refusal> {
        match &r {
            Ok(_) => self.mark_mutated(),
            Err(e) if e.code == Code::IoUnconfirmed => self.mark_mutated(),
            Err(_) => {}
        }
        r.map_err(|e| self.stamp(e))
    }

    /// A refusal after this store wrote anything exits 1, not 3 (architecture section 10).
    pub fn stamp(&self, r: Refusal) -> Refusal {
        if self.mutated() {
            r.after_mutation()
        } else {
            r
        }
    }

    /// A claim must lie inside HOME and outside the manager's own paths, as written and after the
    /// symlinks in its parent directories are resolved. Whether a path in HOME belongs to the
    /// manager is the adapter's evidence, not a guess.
    pub fn check_claim(&self, path: &str) -> Result<(), Refusal> {
        self.check_path(path, false)
    }

    /// Stored paths and writes keep one code for every refusal here, `E_STATE_INVALID`.
    fn check_path(&self, path: &str, new_dir: bool) -> Result<(), Refusal> {
        self.resolve_path(path, new_dir).map_err(|e| {
            if e.code == Code::ParentUnsafe {
                Refusal::new(
                    Code::StateInvalid,
                    format!("{path} is outside HOME or overlaps the manager's own paths"),
                )
            } else {
                e
            }
        })
    }

    /// As [`Store::check_claim`], for the planner: a parent link that leads out of HOME is
    /// `E_PARENT_UNSAFE`, which is not an overlap with the manager's own paths. Every other
    /// overlap is `E_STATE_INVALID`.
    pub fn check_claim_for_plan(&self, path: &str) -> Result<(), Refusal> {
        self.resolve_path(path, false)
    }

    /// A path in a journal entry. A directory the plan created while it was missing may lie
    /// above a reserved path: `~/.local/share` is made before `~/.local/share/<manager>`
    /// exists. It may not be a reserved path or lie inside one. Any other path is a claim.
    fn check_entry_path(&self, e: &JournalEntry, path: &str) -> Result<(), Refusal> {
        self.check_path(path, e.creates_dir())
    }

    fn resolve_path(&self, path: &str, new_dir: bool) -> Result<(), Refusal> {
        let admits = |p: &str| {
            if new_dir {
                self.scope.admits_new_dir(p)
            } else {
                self.scope.admits(p)
            }
        };
        let outside = || {
            Refusal::new(
                Code::StateInvalid,
                format!("{path} is outside HOME or overlaps the manager's own paths"),
            )
        };
        if !admits(path) {
            return Err(outside());
        }
        self.identity_check(path, new_dir)?;
        let p = Path::new(path);
        // `admits` accepted the path, so it is absolute with at least one component: both exist.
        // The refusal is a guard against a change to `valid_path`, and no test can reach it.
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
                    // HOME exists, so the walk up ends at an existing directory before it runs
                    // out of components. The two refusals below guard that invariant and no test
                    // can reach them.
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
            Some(s) if admits(s) => Ok(()),
            // A link in a parent leads out of HOME. That is an unsafe parent, not an overlap with
            // the manager's own paths.
            Some(_) if !full.starts_with(&self.home) => Err(Refusal::new(
                Code::ParentUnsafe,
                format!("a parent directory of {path} leads outside HOME through a link"),
            )),
            _ => Err(outside()),
        }
    }

    /// Repairs a single torn final journal line, the result of a crash or a failed write in the
    /// middle of an append. If the journal does not end in a newline, the bytes after the last
    /// newline are cut off, then one `failed` entry (`op` `repair`) records how many bytes were
    /// removed. This is the first write of the run that needs it. A torn line that is not the
    /// last one, or any other damage, is not repaired: the journal then stays unreadable and
    /// every command refuses it. Returns the number of bytes removed, 0 when nothing was torn.
    pub fn repair_torn_journal(&self) -> Result<usize, Refusal> {
        self.guard(None)?;
        let bytes = self.read_doc("journal.jsonl")?;
        // Nothing torn: no write, so the store is not marked as mutated.
        if bytes.last().is_none_or(|b| *b == b'\n') {
            return Ok(0);
        }
        // The cut is a write: from here the store counts as mutated even if the next step fails.
        self.repair_torn_journal_inner(&bytes)
    }

    fn repair_torn_journal_inner(&self, bytes: &[u8]) -> Result<usize, Refusal> {
        let keep = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        // Everything before the torn line must be a sound journal.
        state::parse_journal(&bytes[..keep])?;
        let removed = bytes.len() - keep;
        // Keep the text before it is cut: nothing is lost if the cut or the record then fails.
        let tail = &bytes[keep..];
        match self
            .state()?
            .publish_new(TORN, tail, &self.fail, "repair-keep")
        {
            // Nothing was written, so the store is not marked as mutated. A copy of the same text
            // is the leftover of a repair that stopped before the cut or before its record: the
            // cut resumes. A copy of other text may be an earlier cut that was never read.
            Ok(false) if self.read_doc(TORN)? == tail => {}
            Ok(false) => {
                return Err(storage(format!(
                    "state/{TORN} holds other text, kept by an earlier repair; nothing was cut. Fix: read it, then remove it by hand"
                )));
            }
            r => {
                self.wrote(r)?;
            }
        }
        self.wrote(self.fail.hit("repair-cut").and_then(|()| {
            self.state()?.truncate_to(
                &self.sys,
                "journal.jsonl",
                u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                u64::try_from(keep).unwrap_or(0),
            )
        }))?;
        // The cut cannot be undone. If the record fails, the removed text stays in `TORN` and
        // the error carries the start of it.
        let record = || -> Result<(), Refusal> {
            let at = now_ns()?;
            self.append_journal(&JournalEntry {
                schema: state::SCHEMA,
                op_id: format!("journal-repair-{at}"),
                op: "repair".into(),
                phase: Phase::Failed,
                at: state::rfc3339(at),
                resource: "journal".into(),
                adapter: "manager".into(),
                class: Class::Normal,
                recovery: None,
                token: None,
                reason: Some(format!("torn line repaired: removed {removed} bytes")),
            })
        };
        record().map_err(|e| {
            let head: String = String::from_utf8_lossy(&bytes[keep..])
                .chars()
                .take(REPAIR_ECHO_CHARS)
                .collect();
            Refusal::new(
                e.code,
                format!(
                    "{}. The torn final line ({removed} bytes) was already cut off and its repair record was not written. The cut text is kept in state/{TORN} (`state check` reports it) and begins: {}",
                    e.detail,
                    head
                ),
            )
            .after_mutation()
        })?;
        // The record exists, so the kept copy is no longer needed. The repair is complete even if
        // the copy cannot be removed: `state check` reports it as a stray entry.
        let _ = self.wrote(self.state()?.remove_entry(TORN, &self.fail));
        Ok(removed)
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
            self.check_entry_path(&e, &o.path)?;
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

    /// `(resource, path, fingerprint)` of every owned output.
    fn ledger_keys(l: &Ledger) -> std::collections::BTreeSet<(String, String, Option<String>)> {
        l.resources
            .iter()
            .flat_map(|r| {
                r.owned_outputs
                    .iter()
                    .map(|o| (r.resource.clone(), o.path.clone(), o.fingerprint.clone()))
            })
            .collect()
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
        let bytes = Self::cap(canonical(l)?)?;
        // An output the stored ledger already holds, unchanged, is not checked again: one
        // unsafe old entry must not stop every later write. New or changed outputs are checked.
        let old = Self::ledger_keys(&self.read_ledger_lenient()?);
        for (_, path, _) in Self::ledger_keys(l).difference(&old) {
            self.check_claim(path)?;
        }
        self.state()?
            .replace_file("ledger.json", &bytes, &self.fail, "ledger")
    }

    /// The origin grants, or none when `state/grants.json` does not exist yet.
    pub fn read_grants(&self) -> Result<Grants, Refusal> {
        match self
            .state()?
            .read_private(&self.sys, GRANTS, state::MAX_BYTES)?
        {
            None => Ok(Grants::default()),
            Some(b) => Grants::parse(&b),
        }
    }

    /// Replaces `state/grants.json` atomically. A grant write is a pre-plan state mutation
    /// (section 10, mixed results); the caller decides what it means for the exit code.
    pub fn write_grants(&self, g: &Grants) -> Result<(), Refusal> {
        self.wrote(self.write_grants_inner(g))
    }

    fn write_grants_inner(&self, g: &Grants) -> Result<(), Refusal> {
        self.guard(None)?;
        let bytes = Self::cap(canonical(g)?)?;
        self.state()?
            .replace_file(GRANTS, &bytes, &self.fail, "grants")
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
        self.wrote(
            self.checked_trial(t).and_then(|(dir, name, bytes)| {
                dir.replace_file(&name, &bytes, &self.fail, "trial")
            }),
        )
    }

    /// Creates a trial record that must not exist yet (`RENAME_EXCL`). `Ok(false)` means a
    /// record of that id was there and nothing was written, so two runs that passed the same
    /// pre-lock check cannot replace each other's record.
    pub fn create_trial(&self, t: &TrialRecord) -> Result<bool, Refusal> {
        let r = self
            .checked_trial(t)
            .and_then(|(dir, name, bytes)| dir.publish_new(&name, &bytes, &self.fail, "trial"));
        match r {
            // Nothing was written, so this is not a mutation: the caller's refusal exits 3.
            Ok(false) => Ok(false),
            r => self.wrote(r),
        }
    }

    /// The trials directory, the file name and the bytes of a record that passed every check.
    fn checked_trial(&self, t: &TrialRecord) -> Result<(Dir, String, Vec<u8>), Refusal> {
        let dir = self
            .guard(Some("trials"))?
            .ok_or_else(|| storage("no trials directory"))?;
        self.check_owner(&t.binding)?;
        t.validate()?;
        t.check_secrets(&self.secret_refs())?;
        let bytes = Self::cap(canonical(t)?)?;
        for p in t
            .owned_outputs
            .iter()
            .chain(t.baseline.paths.iter().map(|b| &b.path))
        {
            self.check_claim(p)?;
        }
        Ok((dir, format!("{}.json", t.trial_id), bytes))
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
        reset_created();
        let made = snaps.child(&self.sys, op_id, true, true);
        // A directory made here is a write even if the payload then fails.
        if created() {
            self.mutated.store(true, Ordering::SeqCst);
        }
        let dir = made?.ok_or_else(|| storage("cannot create the snapshot directory"))?;
        if dir.publish_new(name, bytes, &self.fail, "snapshot")? {
            Ok(())
        } else {
            Err(storage(format!("snapshot {op_id}/{name} already exists")))
        }
    }

    /// Settles each interrupted operation by asking `probe` (the operation's `already_done`
    /// check) and journaling the answer. It never runs an operation again, never edits the ledger
    /// itself (the probe may move an owned fingerprint, see `recover`) and never creates
    /// ownership or delete authority: a finished operation becomes owned only when a normal run
    /// records it. Returns `(op_id, op, phase)` for each entry written.
    pub fn reconcile(
        &self,
        probe: &dyn Fn(&JournalEntry) -> Probe,
    ) -> Result<Vec<(String, String, Phase)>, Refusal> {
        self.guard(None)?;
        let entries = self.read_journal_lenient()?;
        let mut out = Vec::new();
        for i in state::unfinished(&entries) {
            let (phase, why) = match probe(i) {
                Probe::Done => (Phase::Done, "reconciled: the effect is present"),
                Probe::NotDone => (Phase::Failed, "reconciled: the effect is absent"),
                Probe::Unknown => (Phase::Unknown, "reconciled: the effect cannot be observed"),
            };
            let e = JournalEntry {
                phase,
                at: state::rfc3339(now_ns()?),
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
                        for p in t
                            .baseline
                            .paths
                            .iter()
                            .map(|b| &b.path)
                            .chain(&t.owned_outputs)
                        {
                            self.check_claim(p)?;
                        }
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
                        if !state::valid_id(&op) {
                            problems.push(Finding::new(
                                Code::StateInvalid,
                                op_dir.clone(),
                                "not a valid snapshot name",
                            ));
                        }
                        for n in Self::list_dir(&sd, &op_dir, &mut problems) {
                            if tmp(&n) {
                                strays.push(format!("{op_dir}/{n} (interrupted write)"));
                            } else if !state::valid_id(&n) {
                                problems.push(Finding::new(
                                    Code::StateInvalid,
                                    format!("{op_dir}/{n}"),
                                    "not a valid snapshot name",
                                ));
                            } else if let Err(e) = read(&sd, &n) {
                                problems.push(Finding::new(
                                    e.code,
                                    format!("{op_dir}/{n}"),
                                    e.detail,
                                ));
                            }
                        }
                    }
                    _ => strays.push(format!("{op_dir} (unexpected)")),
                }
            }
        }
        if self.state().is_ok() {
            match (self.salt(), self.records_salted_hash()) {
                (Err(e), _) => problems.push(Finding::new(Code::StorageInvalid, "salt", e.detail)),
                (Ok(None), Ok(true)) => problems.push(Finding::new(
                    Code::StateInvalid,
                    "salt",
                    dfm_model::merge::SALT_LOST,
                )),
                // The ledger finding reports the read error itself. This one says what it
                // leaves open.
                (Ok(None), Err(le)) => problems.push(Finding::new(
                    Code::StateInvalid,
                    "salt",
                    format!(
                        "state/salt is missing and the ledger cannot be read ({}), so whether the ledger records a salted hash is unknown",
                        le.detail
                    ),
                )),
                (Ok(_), _) => {}
            }
        }
        if let Ok(state) = self.state() {
            for n in Self::list_dir(state, STATE, &mut problems) {
                let known = ["state.json", "ledger.json", "journal.jsonl", GRANTS, SALT];
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
    if let Ok(entries) = &l.journal {
        for u in state::unresolved(entries) {
            add(
                Code::OpInterrupted,
                "journal.jsonl",
                format!(
                    "operation {} of run {} ended unknown and still blocks its path",
                    u.unknown.op, u.unknown.op_id
                ),
            );
        }
    }
    out
}

/// 32 random bytes from the operating system.
fn random_salt() -> Result<[u8; SALT_BYTES], Refusal> {
    use std::io::Read;
    let mut buf = [0u8; SALT_BYTES];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| crate::fsys::io("read /dev/urandom", &e))?;
    Ok(buf)
}
