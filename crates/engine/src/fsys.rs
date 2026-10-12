//! Storage topology, the writer lock and atomic file operations (docs/state.md).
//!
//! Every operation is relative to an open, verified directory descriptor ([`Dir`]): `openat`,
//! `mkdirat`, `renameat`, `unlinkat` and `fstatat`, all with `O_NOFOLLOW` or
//! `AT_SYMLINK_NOFOLLOW`, through the safe API of `rustix`. A directory is checked on the
//! descriptor it opened (`fstat`), never by path, so swapping a path component after the check
//! cannot redirect a later operation: the descriptor keeps naming the directory that was
//! verified. `unsafe` is denied workspace-wide, with one `expect` in `signal.rs`.
//!
//! What is still path-based: HOME is resolved once with `canonicalize` and then opened (aliases
//! such as `/var` to `/private/var` are accepted above HOME), and a claim's parent directories
//! in HOME are resolved by path when a claim is checked. Both are outside the manager's storage.

use crate::fail::Fail;
use dfm_model::{Code, Refusal};
use rustix::fs::{self as rfs, AtFlags, CWD, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use std::cell::Cell;
use std::fs::{self, File, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

thread_local! {
    /// Set when this thread created a directory or the lock file. A refusal after that happened
    /// after a mutation (exit 1, not 3).
    static CREATED: Cell<bool> = const { Cell::new(false) };
}

/// Clears the creation flag. Called at the start of an operation that may create things.
pub fn reset_created() {
    CREATED.with(|c| c.set(false));
}

/// Whether this thread created a directory or the lock file since [`reset_created`].
pub fn created() -> bool {
    CREATED.with(Cell::get)
}

thread_local! {
    static LISTED: Cell<usize> = const { Cell::new(0) };
}

/// Test seam: how many directory listings this thread has read, so a test can check that a
/// directory is listed once. Not part of the engine's API.
#[doc(hidden)]
pub fn listed() -> usize {
    LISTED.with(Cell::get)
}

/// Test seam, see [`listed`].
#[doc(hidden)]
pub fn reset_listed() {
    LISTED.with(|c| c.set(0));
}

fn note_created() {
    CREATED.with(|c| c.set(true));
}

/// The effective user. The engine names no other OS fact.
#[derive(Clone, Copy, Debug)]
pub struct Sys {
    pub uid: u32,
}

/// Where the manager keeps things, all below one HOME.
#[derive(Clone, Debug)]
pub struct Layout {
    pub home: PathBuf,
    pub config_dir: PathBuf,
    pub state_root: PathBuf,
    pub lock_file: PathBuf,
    /// Manager-owned data (toolchains) and cache (build) roots. Claims may not touch them.
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
}

pub fn storage(detail: impl Into<String>) -> Refusal {
    Refusal::new(Code::StorageInvalid, detail)
}

pub fn io(what: &str, e: &std::io::Error) -> Refusal {
    // The OS message (for example "Operation not permitted (os error 1)") says more than the kind.
    Refusal::new(Code::IoFailed, format!("{what}: {e}"))
}

pub(crate) fn sys_err(what: &str, e: Errno) -> Refusal {
    io(what, &std::io::Error::from(e))
}

fn io_kind(e: Errno) -> String {
    std::io::Error::from(e).kind().to_string()
}

/// The components of `path` below `home`. `home` is the unresolved HOME the layout was built from.
pub fn below(home: &Path, path: &Path) -> Result<Vec<String>, Refusal> {
    let rel = path
        .strip_prefix(home)
        .map_err(|_| storage(format!("{} is outside HOME", path.display())))?;
    rel.components()
        .map(|c| match c {
            Component::Normal(n) => n
                .to_str()
                .map(str::to_string)
                .ok_or_else(|| storage("a path component is not UTF-8")),
            _ => Err(storage(format!("{} is not normalized", path.display()))),
        })
        .collect()
}

/// The longest a file name can be on the supported file system.
pub(crate) const NAME_MAX: usize = 255;

/// The most bytes [`temp_name`] adds to a name: five dots, a process id and a counter of up to
/// 10 digits each, up to 9 digits of nanoseconds and a suffix of up to 5 letters (`stage`). A
/// unit test builds the longest name and compares its length with this value.
pub(crate) const TEMP_NAME_EXTRA: usize = 1 + 1 + 10 + 1 + 10 + 1 + 9 + 1 + 5;

/// A hidden same-directory name for a staged file or link: unique per process and call.
pub(crate) fn temp_name(name: &str, suffix: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    temp_name_of(
        name,
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed),
        nanos,
        suffix,
    )
}

fn temp_name_of(name: &str, pid: u32, n: u32, nanos: u32, suffix: &str) -> String {
    format!(".{name}.{pid}.{n}.{nanos}.{suffix}")
}

/// The refusal for a failed exclusive rename that is not `EEXIST`. `ENOTSUP` and `EINVAL` mean
/// that the file system does not take `RENAME_EXCL`: there is no fallback, so the text names
/// the path and the requirement (docs/state.md, "Known limits").
pub(crate) fn publish_err(path: &Path, e: Errno) -> Refusal {
    if matches!(e, Errno::NOTSUP | Errno::INVAL) {
        return io(
            &format!(
                "publish {} (creating a file needs a file system that supports an exclusive rename, RENAME_EXCL)",
                path.display()
            ),
            &std::io::Error::from(e),
        );
    }
    sys_err("publish", e)
}

/// Renames `from` to `to` in the directory `dir` and fails with `EEXIST` when `to` exists.
pub(crate) fn rename_new(dir: &OwnedFd, from: &str, to: &str) -> Result<(), Errno> {
    rfs::renameat_with(dir, from, dir, to, rfs::RenameFlags::NOREPLACE)
}

pub(crate) fn check_name(name: &str) -> Result<(), Refusal> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(storage(format!("{name:?} is not a plain file name")));
    }
    Ok(())
}

/// Device and inode: the identity of a file or directory, independent of any path to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ident {
    dev: u64,
    ino: u64,
}

pub(crate) fn ident_of(st: &Stat) -> Result<Ident, Refusal> {
    Ok(Ident {
        dev: u64::try_from(st.st_dev).map_err(|_| storage("a device number is out of range"))?,
        ino: st.st_ino,
    })
}

/// What identifies one state of a file without reading it: device, inode, size, and the change
/// and modification times in nanoseconds. A write in place changes the times, a replacement
/// changes the inode, and a user cannot restore the change time. It is a check-then-act guard
/// for a co-owned file, not a content hash (docs/apply.md, "Merge").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    ident: Ident,
    size: u64,
    mtime_ns: i128,
    ctime_ns: i128,
}

fn stamp_of(st: &Stat) -> Result<Stamp, Refusal> {
    let ns = |s: i64, n: i64| i128::from(s) * 1_000_000_000 + i128::from(n);
    Ok(Stamp {
        ident: ident_of(st)?,
        size: u64::try_from(st.st_size).unwrap_or(u64::MAX),
        mtime_ns: ns(st.st_mtime, st.st_mtime_nsec),
        ctime_ns: ns(st.st_ctime, st.st_ctime_nsec),
    })
}

/// The live file of a co-owned destination, read for one merge ([`Dir::read_live`]). `Debug`
/// prints the size of the bytes and never the bytes: they can hold credentials.
pub enum Live {
    Missing,
    /// Not a regular file, owned by another user, or hard-linked. The text says which.
    Unusable(&'static str),
    TooBig,
    File {
        bytes: Vec<u8>,
        mode: u32,
        /// The state the bytes were read from. Publication compares it again before the rename.
        stamp: Stamp,
    },
}

impl std::fmt::Debug for Live {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Live::Missing => f.write_str("Missing"),
            Live::Unusable(why) => write!(f, "Unusable({why})"),
            Live::TooBig => f.write_str("TooBig"),
            Live::File { bytes, mode, .. } => {
                write!(f, "File {{ {} bytes, mode {mode:o} }}", bytes.len())
            }
        }
    }
}

pub(crate) fn mode_bits(st: &Stat) -> u32 {
    u32::from(st.st_mode) & 0o777
}

pub(crate) fn kind(st: &Stat) -> FileType {
    FileType::from_raw_mode(st.st_mode)
}

/// The identity of whatever `path` names, by device and inode. `follow` resolves a symlink at
/// the end. `None` when nothing is there (or the path runs through a file). Used to compare
/// claims with the manager's own paths by identity instead of by spelling.
pub fn path_ident(path: &Path, follow: bool) -> Result<Option<Ident>, Refusal> {
    let m = if follow {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    };
    match m {
        Ok(m) => Ok(Some(Ident {
            dev: m.dev(),
            ino: m.ino(),
        })),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) || e.raw_os_error() == Some(Errno::LOOP.raw_os_error()) =>
        {
            Ok(None)
        }
        Err(e) => Err(io("stat claim", &e)),
    }
}

/// A listing: names, and a count of entries whose name is not UTF-8.
#[derive(Debug, Default)]
pub struct Listing {
    pub names: Vec<String>,
    pub odd: usize,
}

/// What `fstatat` found at one entry, without following it.
#[derive(Debug)]
pub enum Peek {
    Missing,
    Dir {
        mode: u32,
        own: bool,
    },
    File {
        mode: u32,
        own: bool,
        size: u64,
    },
    Link {
        target: String,
        to: PeekTo,
        own: bool,
    },
    Other {
        mode: u32,
        own: bool,
    },
    /// The entry could not be examined; the text is the error kind.
    Unknown(String),
}

/// What a link resolves to when followed once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeekTo {
    Dir,
    File,
    Other,
    Dangling,
    Loop,
}

/// The result of reading an input file ([`Dir::read_input`]).
#[derive(Debug, PartialEq, Eq)]
pub enum Got {
    Bytes(Vec<u8>),
    /// Absent, not a regular file, owned by another user or hard-linked.
    Absent,
    /// Larger than the limit. Nothing was read.
    TooBig,
}

/// An open directory. Operations take a plain entry name and act on this directory only.
#[derive(Debug)]
pub struct Dir {
    pub(crate) fd: OwnedFd,
    /// Where this directory was reached, for messages only. Never used to act.
    shown: PathBuf,
}

impl Dir {
    /// Resolves HOME through every alias, opens it and checks it: a directory owned by the
    /// current user and not writable by group or others.
    pub fn open_home(sys: &Sys, home: &Path) -> Result<Dir, Refusal> {
        let real = fs::canonicalize(home)
            .map_err(|e| storage(format!("{}: {}", home.display(), e.kind())))?;
        let fd = rfs::openat(
            CWD,
            &real,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| sys_err("open home", e))?;
        let st = rfs::fstat(&fd).map_err(|e| sys_err("stat home", e))?;
        if st.st_uid != sys.uid || mode_bits(&st) & 0o022 != 0 {
            return Err(storage(format!(
                "{} is not a directory owned by the current user and closed to group and others",
                real.display()
            )));
        }
        Ok(Dir { fd, shown: real })
    }

    /// Opens any existing directory with no ownership or mode check. For `self-check
    /// --fixture-dir`, where the caller chose the directory.
    pub fn open_plain(path: &Path) -> Result<Dir, Refusal> {
        let real = fs::canonicalize(path)
            .map_err(|e| storage(format!("{}: {}", path.display(), e.kind())))?;
        let fd = rfs::openat(
            CWD,
            &real,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| storage(format!("{}: {}", real.display(), io_kind(e))))?;
        Ok(Dir { fd, shown: real })
    }

    pub fn shown(&self) -> &Path {
        &self.shown
    }

    pub fn try_clone(&self) -> Result<Dir, Refusal> {
        Ok(Dir {
            fd: self
                .fd
                .try_clone()
                .map_err(|e| io("duplicate directory", &e))?,
            shown: self.shown.clone(),
        })
    }

    pub fn ident(&self) -> Result<Ident, Refusal> {
        ident_of(&rfs::fstat(&self.fd).map_err(|e| sys_err("stat directory", e))?)
    }

    pub(crate) fn at(&self, name: &str) -> PathBuf {
        self.shown.join(name)
    }

    /// Explains why `openat(name)` failed for a manager path: a link, a wrong type, or other.
    fn classify(&self, name: &str, e: Errno) -> Refusal {
        let p = self.at(name).display().to_string();
        match rfs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) if kind(&st) == FileType::Symlink => storage(format!(
                "{p} is a symlink; a redirected manager path is refused, not repaired"
            )),
            Ok(st) if kind(&st) != FileType::Directory => storage(format!(
                "{p} is not a directory; a conflicting path type is refused, not replaced"
            )),
            _ => storage(format!("{p}: {}", io_kind(e))),
        }
    }

    /// Opens (or creates, mode 0700) the subdirectory `name` without following a link and
    /// checks it on the descriptor: owned by the current user, and `private` means exactly
    /// 0700 while otherwise it only has to be closed to group and others. `None` when absent
    /// and `create` is not set. Nothing is repaired or replaced.
    pub(crate) fn child(
        &self,
        sys: &Sys,
        name: &str,
        private: bool,
        create: bool,
    ) -> Result<Option<Dir>, Refusal> {
        check_name(name)?;
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = match rfs::openat(&self.fd, name, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) if create => {
                let made = match rfs::mkdirat(&self.fd, name, Mode::from_raw_mode(0o700)) {
                    Ok(()) => {
                        note_created();
                        true
                    }
                    // A concurrent creator is fine; what exists is validated below.
                    Err(Errno::EXIST) => false,
                    Err(e) => {
                        return Err(sys_err(&format!("create {}", self.at(name).display()), e));
                    }
                };
                if made {
                    // By name and before the open: a umask that clears the owner's read bit
                    // would leave a directory this process cannot open.
                    rfs::chmodat(
                        &self.fd,
                        name,
                        Mode::from_raw_mode(0o700),
                        AtFlags::SYMLINK_NOFOLLOW,
                    )
                    .map_err(|e| sys_err("chmod new directory", e))?;
                }
                let fd = rfs::openat(&self.fd, name, flags, Mode::empty())
                    .map_err(|e| self.classify(name, e))?;
                if made {
                    // The new entry must be durable before anything is written inside.
                    self.sync()?;
                }
                fd
            }
            Err(Errno::NOENT) => return Ok(None),
            Err(e) => return Err(self.classify(name, e)),
        };
        let st = rfs::fstat(&fd).map_err(|e| sys_err("stat directory", e))?;
        let p = self.at(name);
        if st.st_uid != sys.uid {
            return Err(storage(format!(
                "{} is not owned by the current user",
                p.display()
            )));
        }
        let mode = mode_bits(&st);
        let ok = if private {
            mode == 0o700
        } else {
            mode & 0o022 == 0
        };
        if !ok {
            return Err(storage(format!(
                "{} has mode {mode:o}, which is not acceptable here",
                p.display()
            )));
        }
        Ok(Some(Dir { fd, shown: p }))
    }

    /// Walks `rel` below this directory with [`Dir::child`]. Components from index
    /// `private_from` on must be exactly 0700. `None` when a component is absent and `create`
    /// is not set.
    pub(crate) fn walk(
        &self,
        sys: &Sys,
        rel: &[String],
        private_from: usize,
        create: bool,
    ) -> Result<Option<Dir>, Refusal> {
        let mut cur = self.try_clone()?;
        for (i, c) in rel.iter().enumerate() {
            match cur.child(sys, c, i >= private_from, create)? {
                Some(next) => cur = next,
                None => return Ok(None),
            }
        }
        Ok(Some(cur))
    }

    /// The identity of the entry `name` itself (a link is not followed). `None` when absent.
    pub fn entry_ident(&self, name: &str) -> Result<Option<Ident>, Refusal> {
        check_name(name)?;
        match rfs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => ident_of(&st).map(Some),
            Err(Errno::NOENT) => Ok(None),
            Err(e) => Err(sys_err("stat entry", e)),
        }
    }

    /// Checks the held descriptor again: still owned by the current user, and `private` means
    /// exactly 0700, otherwise closed to group and others. Used before each write.
    pub fn recheck(&self, sys: &Sys, private: bool) -> Result<(), Refusal> {
        let st = rfs::fstat(&self.fd).map_err(|e| sys_err("stat directory", e))?;
        let mode = mode_bits(&st);
        let ok = st.st_uid == sys.uid
            && if private {
                mode == 0o700
            } else {
                mode & 0o022 == 0
            };
        if ok {
            Ok(())
        } else {
            Err(storage(format!(
                "{} changed owner or mode to {mode:o} while open",
                self.shown.display()
            )))
        }
    }

    /// Flushes this directory's entries to stable storage. `File::sync_all` asks for
    /// `F_FULLFSYNC` on macOS (plain `fsync` does not flush the drive cache there). A volume or
    /// directory that rejects that request gets a plain `fsync` instead. Whether the data
    /// reaches the medium cannot be observed from a test, and tests that abort the process
    /// leave the page cache intact, so none of them shows power-loss ordering.
    pub fn sync(&self) -> Result<(), Refusal> {
        let f = File::from(
            self.fd
                .try_clone()
                .map_err(|e| io("duplicate directory", &e))?,
        );
        match f.sync_all() {
            Ok(()) => Ok(()),
            Err(e)
                if [Errno::NOTSUP, Errno::NOTTY, Errno::INVAL]
                    .iter()
                    .any(|x| e.raw_os_error() == Some(x.raw_os_error())) =>
            {
                rfs::fsync(&self.fd).map_err(|e| sys_err("sync directory", e))
            }
            Err(e) => Err(io("sync directory", &e)),
        }
    }

    /// Reads a manager-owned file: no symlink, regular, owned by the current user, mode 0600,
    /// at most `limit` bytes. `None` when it does not exist. All checks use the descriptor.
    pub fn read_private(
        &self,
        sys: &Sys,
        name: &str,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, Refusal> {
        check_name(name)?;
        let bad = |d: String| storage(format!("{}: {d}", self.at(name).display()));
        let fd = match rfs::openat(
            &self.fd,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(Errno::LOOP) => return Err(bad("is a symlink".into())),
            Err(e) => return Err(bad(io_kind(e))),
        };
        let st = rfs::fstat(&fd).map_err(|e| sys_err("stat file", e))?;
        if kind(&st) != FileType::RegularFile {
            return Err(bad("not a regular file".into()));
        }
        if st.st_uid != sys.uid {
            return Err(bad("not owned by the current user".into()));
        }
        if u64::from(st.st_nlink) != 1 {
            return Err(bad("has more than one hard link".into()));
        }
        if mode_bits(&st) != 0o600 {
            return Err(bad(format!("mode is {:o}, expected 0600", mode_bits(&st))));
        }
        let mut buf = Vec::new();
        File::from(fd)
            .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
            .read_to_end(&mut buf)
            .map_err(|e| bad(e.kind().to_string()))?;
        if buf.len() > limit {
            return Err(Refusal::new(
                Code::LimitExceeded,
                format!("{} exceeds {limit} bytes", self.at(name).display()),
            ));
        }
        Ok(Some(buf))
    }

    /// A same-directory temporary file, fully written and synced, named so that readers ignore it.
    fn write_temp(&self, name: &str, bytes: &[u8]) -> Result<String, Refusal> {
        self.write_temp_mode(name, bytes, 0o600)
    }

    /// As [`Dir::write_temp`], with the mode the published file will have. The mode is set
    /// before any content is written, so the file is never wider than `mode`.
    pub(crate) fn write_temp_mode(
        &self,
        name: &str,
        bytes: &[u8],
        mode: u32,
    ) -> Result<String, Refusal> {
        let tmp = temp_name(name, "tmp");
        let raw = rfs::RawMode::try_from(mode)
            .map_err(|_| storage(format!("file mode {mode:o} is out of range")))?;
        let fd = rfs::openat(
            &self.fd,
            &tmp,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|e| sys_err("create temporary file", e))?;
        let written = rfs::fchmod(&fd, Mode::from_raw_mode(raw))
            .map_err(std::io::Error::from)
            .and_then(|()| {
                let mut f = File::from(fd);
                f.write_all(bytes)?;
                f.sync_all()
            });
        match written {
            Ok(()) => Ok(tmp),
            Err(e) => {
                self.drop_temp(&tmp);
                Err(io("write temporary file", &e))
            }
        }
    }

    /// Removes a temporary name. Used where a failure leaves nothing that matters.
    pub(crate) fn drop_temp(&self, tmp: &str) {
        let _ = rfs::unlinkat(&self.fd, tmp, AtFlags::empty());
    }

    fn require_plain(&self, name: &str) -> Result<(), Refusal> {
        match rfs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) if kind(&st) == FileType::RegularFile => Ok(()),
            Ok(_) => Err(storage(format!(
                "{} exists and is not a regular file",
                self.at(name).display()
            ))),
            Err(Errno::NOENT) => Ok(()),
            Err(e) => Err(sys_err("stat target", e)),
        }
    }

    /// Atomic replacement: temporary file, fsync, `renameat`, directory fsync. A failure before
    /// the rename leaves the previous file, which is the last good state. A failure after it is
    /// `E_IO_UNCONFIRMED`: the new file is in place and only its durability is unknown.
    /// Failure points are `<label>-before-write`, `-after-tmp` and `-after-rename`.
    pub(crate) fn replace_file(
        &self,
        name: &str,
        bytes: &[u8],
        fail: &Fail,
        label: &str,
    ) -> Result<(), Refusal> {
        check_name(name)?;
        self.require_plain(name)?;
        fail.hit(&format!("{label}-before-write"))?;
        let tmp = self.write_temp(name, bytes)?;
        let step = || -> Result<(), Refusal> {
            fail.hit(&format!("{label}-after-tmp"))?;
            rfs::renameat(&self.fd, &tmp, &self.fd, name).map_err(|e| sys_err("rename", e))
        };
        if let Err(e) = step() {
            self.drop_temp(&tmp);
            return Err(e);
        }
        unconfirmed(
            fail.hit(&format!("{label}-after-rename"))
                .and_then(|()| self.sync()),
        )
    }

    /// Create-only publication: the staged file is renamed with `RENAME_EXCL`, which never
    /// replaces and leaves one link, so there is no moment with two names for the file.
    /// `Ok(false)` means the name existed and nothing was written. Failure points are as for
    /// [`Dir::replace_file`], with `-after-tmp` before the rename.
    pub fn publish_new(
        &self,
        name: &str,
        bytes: &[u8],
        fail: &Fail,
        label: &str,
    ) -> Result<bool, Refusal> {
        check_name(name)?;
        fail.hit(&format!("{label}-before-write"))?;
        let tmp = self.write_temp(name, bytes)?;
        let step = || -> Result<bool, Refusal> {
            fail.hit(&format!("{label}-after-tmp"))?;
            match rename_new(&self.fd, &tmp, name) {
                Ok(()) => Ok(true),
                Err(Errno::EXIST) => Ok(false),
                Err(e) => Err(publish_err(&self.at(name), e)),
            }
        };
        let r = step();
        if r.as_ref().is_ok_and(|created| *created) {
            unconfirmed(
                fail.hit(&format!("{label}-after-rename"))
                    .and_then(|()| self.sync()),
            )?;
        } else {
            self.drop_temp(&tmp);
        }
        r
    }

    /// Opens a journal-like file for appending: no symlink, a private regular file with one link
    /// owned by the current user. Returns the descriptor and the size.
    fn open_append(&self, sys: &Sys, name: &str, append: bool) -> Result<(OwnedFd, u64), Refusal> {
        check_name(name)?;
        let bad = |d: String| storage(format!("{}: {d}", self.at(name).display()));
        let fd = rfs::openat(
            &self.fd,
            name,
            // O_NONBLOCK so a FIFO planted at this name cannot hang the open.
            OFlags::RDWR
                | if append {
                    OFlags::APPEND
                } else {
                    OFlags::empty()
                }
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK
                | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| match e {
            Errno::LOOP => bad("is a symlink".into()),
            e => bad(io_kind(e)),
        })?;
        let st = rfs::fstat(&fd).map_err(|e| sys_err("stat file", e))?;
        if kind(&st) != FileType::RegularFile
            || st.st_uid != sys.uid
            || mode_bits(&st) != 0o600
            || u64::from(st.st_nlink) != 1
        {
            return Err(bad(
                "must be a private regular file with one link owned by the current user".into(),
            ));
        }
        let size = u64::try_from(st.st_size).map_err(|_| bad("has a negative size".into()))?;
        Ok((fd, size))
    }

    /// Cuts `name` back to `len` bytes and syncs it. Used only to remove a torn final journal
    /// line. A failure of the cut is an unconfirmed outcome: the file may or may not be shorter.
    /// `read_size` is the size the caller judged; a file of another size was changed since.
    pub fn truncate_to(
        &self,
        sys: &Sys,
        name: &str,
        read_size: u64,
        len: u64,
    ) -> Result<(), Refusal> {
        let (fd, size) = self.open_append(sys, name, false)?;
        if size != read_size || len > size {
            return Err(storage(format!(
                "{}: the file changed while it was read ({read_size} bytes read, {size} now)",
                self.at(name).display()
            )));
        }
        let f = File::from(fd);
        f.set_len(len).and_then(|()| f.sync_all()).map_err(|e| {
            Refusal::new(
                Code::IoUnconfirmed,
                format!("cutting the torn journal line is unconfirmed: {e}"),
            )
        })
    }

    /// Appends one complete line. The file must exist, be private, have one link and end with a
    /// newline (or be empty), so a torn earlier line is never extended. Failure points:
    /// `<label>-before-write` and `<label>-after-write`.
    #[doc(hidden)]
    pub fn append_line(
        &self,
        sys: &Sys,
        name: &str,
        line: &[u8],
        max_total: u64,
        fail: &Fail,
        label: &str,
    ) -> Result<(), Refusal> {
        let (fd, size) = self.open_append(sys, name, true)?;
        if size.saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX)) > max_total {
            return Err(Refusal::new(
                Code::LimitExceeded,
                format!(
                    "{} would pass {max_total} bytes; it is not appended",
                    self.at(name).display()
                ),
            ));
        }
        let mut f = File::from(fd);
        if size > 0 {
            let mut last = [0u8; 1];
            f.seek(SeekFrom::End(-1))
                .and_then(|_| f.read_exact(&mut last))
                .map_err(|e| io("read journal tail", &e))?;
            if last[0] != b'\n' {
                return Err(Refusal::new(
                    Code::StateInvalid,
                    format!(
                        "{} ends with a torn line; it is reported, not extended",
                        self.at(name).display()
                    ),
                ));
            }
        }
        fail.hit(&format!("{label}-before-write"))?;
        write_line(&mut f, line)?;
        // The line is written. A failure from here on means its durability is unconfirmed.
        unconfirmed(
            f.sync_all()
                .map_err(|e| io("sync journal", &e))
                .and_then(|()| fail.hit(&format!("{label}-after-write"))),
        )
    }

    /// Opens the subdirectory `name` for reading without following a link and without any
    /// owner or mode check. `None` when `name` is absent or is not a directory (a link, a file).
    /// For observation only: nothing is created and nothing is judged here.
    pub fn peek_dir(&self, name: &str) -> Result<Option<Dir>, Refusal> {
        check_name(name)?;
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        match rfs::openat(&self.fd, name, flags, Mode::empty()) {
            Ok(fd) => Ok(Some(Dir {
                fd,
                shown: self.at(name),
            })),
            Err(Errno::NOENT | Errno::NOTDIR | Errno::LOOP) => Ok(None),
            Err(e) => Err(sys_err(&format!("open {}", self.at(name).display()), e)),
        }
    }

    /// Looks at the entry `name` without following it (`fstatat` with `AT_SYMLINK_NOFOLLOW`).
    /// A link is read, and followed once from this directory to say what it resolves to.
    pub fn peek(&self, sys: &Sys, name: &str) -> Result<Peek, Refusal> {
        check_name(name)?;
        let st = match rfs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => st,
            Err(Errno::NOENT) => return Ok(Peek::Missing),
            Err(e) => return Ok(Peek::Unknown(io_kind(e))),
        };
        let (mode, own) = (mode_bits(&st), st.st_uid == sys.uid);
        Ok(match kind(&st) {
            FileType::Directory => Peek::Dir { mode, own },
            FileType::RegularFile => Peek::File {
                mode,
                own,
                size: u64::try_from(st.st_size).unwrap_or(u64::MAX),
            },
            FileType::Symlink => {
                let target = match rfs::readlinkat(&self.fd, name, Vec::new()) {
                    Ok(t) => t.to_string_lossy().into_owned(),
                    Err(e) => return Ok(Peek::Unknown(io_kind(e))),
                };
                let to = match rfs::statat(&self.fd, name, AtFlags::empty()) {
                    Ok(f) => match kind(&f) {
                        FileType::Directory => PeekTo::Dir,
                        FileType::RegularFile => PeekTo::File,
                        _ => PeekTo::Other,
                    },
                    Err(Errno::NOENT | Errno::NOTDIR) => PeekTo::Dangling,
                    Err(Errno::LOOP) => PeekTo::Loop,
                    Err(e) => return Ok(Peek::Unknown(io_kind(e))),
                };
                Peek::Link { target, to, own }
            }
            _ => Peek::Other { mode, own },
        })
    }

    /// Reads at most `limit` bytes of the regular file `name` without following a link.
    /// `None` when it is absent, not a regular file, or larger than `limit`.
    pub fn read_plain(&self, name: &str, limit: u64) -> Result<Option<Vec<u8>>, Refusal> {
        check_name(name)?;
        let fd = match rfs::openat(
            &self.fd,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::NOENT | Errno::LOOP | Errno::NOTDIR) => return Ok(None),
            Err(e) => return Err(sys_err(&format!("open {}", self.at(name).display()), e)),
        };
        let st = rfs::fstat(&fd).map_err(|e| sys_err("stat file", e))?;
        if kind(&st) != FileType::RegularFile {
            return Ok(None);
        }
        let mut buf = Vec::new();
        File::from(fd)
            .take(limit.saturating_add(1))
            .read_to_end(&mut buf)
            .map_err(|e| io("read file", &e))?;
        Ok((u64::try_from(buf.len()).unwrap_or(u64::MAX) <= limit).then_some(buf))
    }

    /// Reads the live file `name` of a co-owned destination from one descriptor opened without
    /// following a link, with its mode and a [`Stamp`] taken after the read. The stamp before and
    /// after the read must agree, so a writer that overlaps the read is a conflict, not a torn
    /// result. The file must be regular, owned by the current user, with one hard link.
    pub fn read_live(&self, sys: &Sys, name: &str, limit: u64) -> Result<Live, Refusal> {
        check_name(name)?;
        let fd = match rfs::openat(
            &self.fd,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(Live::Missing),
            Err(Errno::LOOP) => return Ok(Live::Unusable("is a link")),
            Err(Errno::NOTDIR) => return Ok(Live::Unusable("is not reachable as a file")),
            Err(e) => return Err(sys_err(&format!("open {}", self.at(name).display()), e)),
        };
        let st = rfs::fstat(&fd).map_err(|e| sys_err("stat file", e))?;
        if kind(&st) != FileType::RegularFile {
            return Ok(Live::Unusable("is not a regular file"));
        }
        if st.st_uid != sys.uid {
            return Ok(Live::Unusable("is owned by another user"));
        }
        if u64::from(st.st_nlink) != 1 {
            return Ok(Live::Unusable("has more than one hard link"));
        }
        if u64::try_from(st.st_size).unwrap_or(u64::MAX) > limit {
            return Ok(Live::TooBig);
        }
        let mut buf = Vec::new();
        let mut f = File::from(fd);
        (&mut f)
            .take(limit.saturating_add(1))
            .read_to_end(&mut buf)
            .map_err(|e| io("read file", &e))?;
        if u64::try_from(buf.len()).unwrap_or(u64::MAX) > limit {
            return Ok(Live::TooBig);
        }
        let after = rfs::fstat(&f).map_err(|e| sys_err("stat file", e))?;
        let (a, b) = (stamp_of(&st)?, stamp_of(&after)?);
        if a != b {
            return Err(Refusal::new(
                Code::Conflict,
                format!(
                    "{} changed while it was read; another program is writing it. Fix: close the program or retry",
                    self.at(name).display()
                ),
            ));
        }
        Ok(Live::File {
            bytes: buf,
            mode: mode_bits(&after),
            stamp: b,
        })
    }

    /// The [`Stamp`] of the entry `name` itself, without following a link. `None` when absent.
    pub(crate) fn entry_stamp(&self, name: &str) -> Result<Option<Stamp>, Refusal> {
        check_name(name)?;
        match rfs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => stamp_of(&st).map(Some),
            Err(Errno::NOENT) => Ok(None),
            Err(e) => Err(sys_err("stat entry", e)),
        }
    }

    /// Reads an input of a generated file: the regular file `name`, opened without following a
    /// link. The file must be owned by the current user and have one hard link, so a file
    /// planted as a second name for someone else's data is not taken for an input. The size is
    /// checked on the descriptor before anything is read, so a file over `limit` is never
    /// allocated.
    pub fn read_input(&self, sys: &Sys, name: &str, limit: u64) -> Result<Got, Refusal> {
        check_name(name)?;
        let fd = match rfs::openat(
            &self.fd,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::NOENT | Errno::LOOP | Errno::NOTDIR) => return Ok(Got::Absent),
            Err(e) => return Err(sys_err(&format!("open {}", self.at(name).display()), e)),
        };
        let st = rfs::fstat(&fd).map_err(|e| sys_err("stat file", e))?;
        if kind(&st) != FileType::RegularFile || st.st_uid != sys.uid || u64::from(st.st_nlink) != 1
        {
            return Ok(Got::Absent);
        }
        if u64::try_from(st.st_size).unwrap_or(u64::MAX) > limit {
            return Ok(Got::TooBig);
        }
        let mut buf = Vec::new();
        File::from(fd)
            .take(limit.saturating_add(1))
            .read_to_end(&mut buf)
            .map_err(|e| io("read file", &e))?;
        Ok(if u64::try_from(buf.len()).unwrap_or(u64::MAX) <= limit {
            Got::Bytes(buf)
        } else {
            Got::TooBig
        })
    }

    /// As [`Dir::read_input`] for the `/`-separated relative path `rel`. Every parent is opened
    /// without following a link. A missing or non-directory parent is `Absent`.
    pub fn read_input_below(&self, sys: &Sys, rel: &str, limit: u64) -> Result<Got, Refusal> {
        let mut parts: Vec<&str> = rel.split('/').collect();
        let Some(name) = parts.pop() else {
            return Ok(Got::Absent);
        };
        let mut cur = self.try_clone()?;
        for p in parts {
            cur = match cur.peek_dir(p)? {
                Some(next) => next,
                None => return Ok(Got::Absent),
            };
        }
        cur.read_input(sys, name, limit)
    }

    /// Lists this directory. Entry names that are not UTF-8 are counted, not dropped silently.
    pub fn list(&self) -> Result<Listing, Refusal> {
        self.list_upto(usize::MAX)
    }

    /// As [`Dir::list`], but stops after `max + 1` names, so a huge directory is not read into
    /// memory. More than `max` names in the result means the listing was cut.
    pub fn list_upto(&self, max: usize) -> Result<Listing, Refusal> {
        LISTED.with(|c| c.set(c.get() + 1));
        let mut rd = rfs::Dir::read_from(&self.fd).map_err(|e| sys_err("list directory", e))?;
        rd.rewind();
        let mut out = Listing::default();
        for entry in rd {
            let entry = entry.map_err(|e| sys_err("read directory entry", e))?;
            let n = entry.file_name().to_bytes();
            if n == b"." || n == b".." {
                continue;
            }
            match std::str::from_utf8(n) {
                Ok(s) => out.names.push(s.to_string()),
                Err(_) => out.odd += 1,
            }
            if out.names.len() > max {
                break;
            }
        }
        out.names.sort();
        Ok(out)
    }

    /// Renames an entry of this directory. The target must not be a non-empty directory.
    pub(crate) fn rename(&self, from: &str, to: &str) -> Result<(), Refusal> {
        check_name(from)?;
        check_name(to)?;
        rfs::renameat(&self.fd, from, &self.fd, to).map_err(|e| sys_err("rename", e))
    }

    /// Removes the directory `name` and everything below it without following any link.
    pub(crate) fn remove_tree(&self, sys: &Sys, name: &str) -> Result<(), Refusal> {
        let Some(d) = self.child(sys, name, false, false)? else {
            return Ok(());
        };
        let listing = d.list()?;
        if listing.odd > 0 {
            return Err(storage(format!(
                "{} has an entry that is not UTF-8; it is not removed",
                d.shown.display()
            )));
        }
        for n in &listing.names {
            match rfs::statat(&d.fd, n.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) if kind(&st) == FileType::Directory => d.remove_tree(sys, n)?,
                Ok(_) => rfs::unlinkat(&d.fd, n.as_str(), AtFlags::empty())
                    .map_err(|e| sys_err("remove file", e))?,
                Err(Errno::NOENT) => {}
                Err(e) => return Err(sys_err("stat entry", e)),
            }
        }
        rfs::unlinkat(&self.fd, name, AtFlags::REMOVEDIR)
            .map_err(|e| sys_err("remove directory", e))
    }
}

/// Opens the directory that holds the binding, below HOME and without following a link. The
/// config directory is shared with other tools, so an existing one only has to be closed to
/// group and others; one this call creates is 0700. `None` when absent and `create` is not set.
pub fn open_config(sys: &Sys, layout: &Layout, create: bool) -> Result<Option<Dir>, Refusal> {
    let home = Dir::open_home(sys, &layout.home)?;
    let rel = below(&layout.home, &layout.config_dir)?;
    home.walk(sys, &rel, rel.len(), create)
}

/// Writes one journal line. A failure here can leave part of the line in the file, so it is an
/// unconfirmed outcome (the journal may end in a torn line), not an ordinary I/O failure.
fn write_line(w: &mut impl Write, line: &[u8]) -> Result<(), Refusal> {
    w.write_all(line).map_err(|e| {
        Refusal::new(
            Code::IoUnconfirmed,
            format!(
                "the journal append failed part way and the journal may end in a torn line: {e}"
            ),
        )
    })
}

/// A failure after the commit point: the new content is visible, durability is not confirmed.
pub fn unconfirmed(r: Result<(), Refusal>) -> Result<(), Refusal> {
    r.map_err(|e| {
        Refusal::new(
            Code::IoUnconfirmed,
            format!(
                "the new file is in place but its durability is unconfirmed: {}",
                e.detail
            ),
        )
    })
}

/// The advisory writer lock (`flock`). The kernel releases it when the process ends, so an
/// abrupt exit cannot leave it held. Dropping the value releases it too.
///
/// The lock file is opened relative to a held directory descriptor. A lock on a deleted or
/// replaced file protects nothing, so the holder compares the file it locked with the entry in
/// that directory before each write ([`Lock::verify`]).
#[derive(Debug)]
pub struct Lock {
    file: File,
    parent: Dir,
    name: String,
    ident: Ident,
}

impl Lock {
    /// Takes the exclusive lock without waiting. A second holder gets `E_LOCKED`.
    pub fn acquire(sys: &Sys, parent: &Dir, name: &str) -> Result<Lock, Refusal> {
        check_name(name)?;
        let bad = |d: String| storage(format!("{}: {d}", parent.at(name).display()));
        // O_NONBLOCK so a FIFO planted at this name cannot hang the open. Open the existing file
        // first. If it is absent, create it exclusively, and if another process created it in
        // between (EEXIST), open it again. The loop is bounded.
        let flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let mode = Mode::from_raw_mode(0o600);
        let mut attempts = 0;
        let fd = loop {
            attempts += 1;
            match rfs::openat(&parent.fd, name, flags, Mode::empty()) {
                Ok(fd) => break fd,
                Err(Errno::LOOP) => return Err(bad("is a symlink".into())),
                Err(Errno::NOENT) => {}
                Err(e) => return Err(bad(io_kind(e))),
            }
            match rfs::openat(
                &parent.fd,
                name,
                flags | OFlags::CREATE | OFlags::EXCL,
                mode,
            ) {
                Ok(fd) => break fd,
                Err(Errno::EXIST) if attempts < 8 => {}
                Err(e) => return Err(bad(io_kind(e))),
            }
        };
        let st = rfs::fstat(&fd).map_err(|e| sys_err("stat lock", e))?;
        if kind(&st) != FileType::RegularFile || st.st_uid != sys.uid {
            return Err(bad(
                "must be a regular file owned by the current user".into()
            ));
        }
        if u64::from(st.st_nlink) != 1 {
            return Err(bad("has more than one hard link".into()));
        }
        let mode = mode_bits(&st);
        if mode != 0o600 {
            // A umask can clear bits of the creation mode. Tightening to 0600 is allowed, a
            // looser mode is refused.
            if mode & !0o600 != 0 {
                return Err(bad(format!("mode is {mode:o}, expected 0600")));
            }
            rfs::fchmod(&fd, Mode::from_raw_mode(0o600)).map_err(|e| sys_err("chmod lock", e))?;
        }
        let ident = ident_of(&st)?;
        let mut file = File::from(fd);
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let mut holder = String::new();
                let _ = file.read_to_string(&mut holder);
                return Err(Refusal::new(
                    Code::Locked,
                    format!("another command holds the manager lock ({})", holder.trim()),
                ));
            }
            Err(TryLockError::Error(e)) => return Err(io("lock", &e)),
        }
        let lock = Lock {
            file,
            parent: parent.try_clone()?,
            name: name.to_string(),
            ident,
        };
        lock.verify()?;
        // Informational only: the lock is the flock, not these bytes.
        let since = crate::store::now_unix().map_or_else(|_| "unknown".into(), |t| t.to_string());
        let note = format!("pid {} since {since}\n", std::process::id());
        let mut f = &lock.file;
        f.set_len(0)
            .and_then(|()| f.write_all(note.as_bytes()))
            .map_err(|e| io("write lock note", &e))?;
        Ok(lock)
    }

    /// Fails unless the entry still names the file this lock holds. Called before every write.
    pub fn verify(&self) -> Result<(), Refusal> {
        let lost = |d: &str| {
            Refusal::new(
                Code::Locked,
                format!(
                    "{}: {d}; this writer must stop",
                    self.parent.at(&self.name).display()
                ),
            )
        };
        match self.parent.entry_ident(&self.name)? {
            Some(i) if i == self.ident => Ok(()),
            Some(_) => Err(lost("the lock file was replaced")),
            None => Err(lost("the lock file was removed")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer that accepts a few bytes and then fails, like a full disk.
    struct Partial(Vec<u8>);

    impl Write for Partial {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.0.is_empty() {
                let n = buf.len().min(3);
                self.0.extend_from_slice(&buf[..n]);
                Ok(n)
            } else {
                Err(std::io::Error::from_raw_os_error(28))
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_partial_journal_write_is_an_unconfirmed_outcome() {
        let mut w = Partial(Vec::new());
        let r = write_line(&mut w, b"{\"a\":1}\n").unwrap_err();
        assert_eq!(w.0, b"{\"a");
        assert_eq!(r.code, Code::IoUnconfirmed);
        assert!(r.detail.contains("torn line"), "{}", r.detail);
    }

    #[test]
    fn the_longest_staged_name_fits_the_reserved_extra() {
        // The widest value of every field, and the longest suffix in use.
        let name = temp_name_of("", u32::MAX, u32::MAX, 999_999_999, "stage");
        assert_eq!(name.len(), TEMP_NAME_EXTRA, "{name}");
        // A real name for any suffix in use stays within the reservation too.
        for suffix in ["stage", "tmp"] {
            assert!(temp_name("", suffix).len() <= TEMP_NAME_EXTRA);
        }
    }

    #[test]
    fn a_file_system_without_exclusive_rename_is_named_in_the_refusal() {
        for e in [Errno::NOTSUP, Errno::INVAL] {
            let r = publish_err(Path::new("/h/.config/a"), e);
            assert_eq!(r.code, Code::IoFailed);
            assert!(r.detail.contains("/h/.config/a"), "{}", r.detail);
            assert!(r.detail.contains("RENAME_EXCL"), "{}", r.detail);
        }
        let other = publish_err(Path::new("/h/a"), Errno::ACCESS);
        assert!(!other.detail.contains("RENAME_EXCL"), "{}", other.detail);
    }
}
