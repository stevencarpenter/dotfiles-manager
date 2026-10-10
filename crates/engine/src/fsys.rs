//! Storage topology, the writer lock and atomic file operations (docs/state.md).
//!
//! Every operation is relative to an open, verified directory descriptor ([`Dir`]): `openat`,
//! `mkdirat`, `renameat`, `linkat`, `unlinkat` and `fstatat`, all with `O_NOFOLLOW` or
//! `AT_SYMLINK_NOFOLLOW`, through the safe API of `rustix`. A directory is checked on the
//! descriptor it opened (`fstat`), never by path, so swapping a path component after the check
//! cannot redirect a later operation: the descriptor keeps naming the directory that was
//! verified. `unsafe` stays forbidden in this workspace.
//!
//! What is still path-based: HOME is resolved once with `canonicalize` and then opened (aliases
//! such as `/var` to `/private/var` are accepted above HOME), and a claim's parent directories
//! in HOME are resolved by path when a claim is checked. Both are outside the manager's storage.

use crate::fail::Fail;
use dfm_model::{Code, Refusal};
use rustix::fs::{self as rfs, AtFlags, CWD, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use std::fs::{self, File, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

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

fn sys_err(what: &str, e: Errno) -> Refusal {
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

fn check_name(name: &str) -> Result<(), Refusal> {
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

fn ident_of(st: &Stat) -> Result<Ident, Refusal> {
    Ok(Ident {
        dev: u64::try_from(st.st_dev).map_err(|_| storage("a device number is out of range"))?,
        ino: st.st_ino,
    })
}

fn mode_bits(st: &Stat) -> u32 {
    u32::from(st.st_mode) & 0o777
}

fn kind(st: &Stat) -> FileType {
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

/// An open directory. Operations take a plain entry name and act on this directory only.
#[derive(Debug)]
pub struct Dir {
    fd: OwnedFd,
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

    fn at(&self, name: &str) -> PathBuf {
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
    pub fn child(
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
                    Ok(()) => true,
                    // A concurrent creator is fine; what exists is validated below.
                    Err(Errno::EXIST) => false,
                    Err(e) => {
                        return Err(sys_err(&format!("create {}", self.at(name).display()), e));
                    }
                };
                let fd = rfs::openat(&self.fd, name, flags, Mode::empty())
                    .map_err(|e| self.classify(name, e))?;
                if made {
                    rfs::fchmod(&fd, Mode::from_raw_mode(0o700))
                        .map_err(|e| sys_err("chmod new directory", e))?;
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
    pub fn walk(
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

    /// Flushes this directory's entries to stable storage. `File::sync_all` is `F_FULLFSYNC` on
    /// macOS (plain `fsync` does not flush the drive cache there), and an interrupted call is
    /// retried. Tests that abort the process cannot prove power-loss ordering: an abort leaves
    /// the page cache intact.
    pub fn sync(&self) -> Result<(), Refusal> {
        let f = File::from(
            self.fd
                .try_clone()
                .map_err(|e| io("duplicate directory", &e))?,
        );
        let mut tries = 0;
        loop {
            match f.sync_all() {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted && tries < 5 => tries += 1,
                Err(e) => return Err(io("sync directory", &e)),
            }
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
        static N: AtomicU32 = AtomicU32::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let tmp = format!(
            ".{name}.{}.{}.{nanos}.tmp",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        );
        let fd = rfs::openat(
            &self.fd,
            &tmp,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|e| sys_err("create temporary file", e))?;
        let written = rfs::fchmod(&fd, Mode::from_raw_mode(0o600))
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
    fn drop_temp(&self, tmp: &str) {
        let _ = rfs::unlinkat(&self.fd, tmp, AtFlags::empty());
    }

    /// Removes the temporary name after a successful `linkat`. A failure leaves the published
    /// file with two links, which the append check would later refuse, so it is reported.
    fn drop_linked_temp(&self, tmp: &str) -> Result<(), Refusal> {
        rfs::unlinkat(&self.fd, tmp, AtFlags::empty()).map_err(|e| {
            Refusal::new(
                Code::IoUnconfirmed,
                format!(
                    "published, but the temporary name {tmp} could not be removed: {}",
                    std::io::Error::from(e)
                ),
            )
        })
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
    pub fn replace_file(
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

    /// Create-only publication (`linkat` never replaces). `Ok(false)` means the name existed and
    /// nothing was written. Failure points are as for [`Dir::replace_file`], with `-after-tmp`
    /// before the link.
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
            match rfs::linkat(&self.fd, &tmp, &self.fd, name, AtFlags::empty()) {
                Ok(()) => Ok(true),
                Err(Errno::EXIST) => Ok(false),
                Err(e) => Err(sys_err("publish", e)),
            }
        };
        let r = step();
        if r.as_ref().is_ok_and(|created| *created) {
            self.drop_linked_temp(&tmp)?;
            unconfirmed(
                fail.hit(&format!("{label}-after-rename"))
                    .and_then(|()| self.sync()),
            )?;
        } else {
            self.drop_temp(&tmp);
        }
        r
    }

    /// Appends one complete line. The file must exist, be private, have one link and end with a
    /// newline (or be empty), so a torn earlier line is never extended. Failure points:
    /// `<label>-before-write` and `<label>-after-write`.
    pub fn append_line(
        &self,
        sys: &Sys,
        name: &str,
        line: &[u8],
        max_total: u64,
        fail: &Fail,
        label: &str,
    ) -> Result<(), Refusal> {
        check_name(name)?;
        let bad = |d: String| storage(format!("{}: {d}", self.at(name).display()));
        let fd = rfs::openat(
            &self.fd,
            name,
            // O_NONBLOCK so a FIFO planted at this name cannot hang the open.
            OFlags::RDWR | OFlags::APPEND | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
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
        f.write_all(line).map_err(|e| io("append", &e))?;
        // The line is written. A failure from here on means its durability is unconfirmed.
        unconfirmed(
            f.sync_all()
                .map_err(|e| io("sync journal", &e))
                .and_then(|()| fail.hit(&format!("{label}-after-write"))),
        )
    }

    /// Lists this directory. Entry names that are not UTF-8 are counted, not dropped silently.
    pub fn list(&self) -> Result<Listing, Refusal> {
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
        }
        out.names.sort();
        Ok(out)
    }

    /// Renames an entry of this directory. The target must not be a non-empty directory.
    pub fn rename(&self, from: &str, to: &str) -> Result<(), Refusal> {
        check_name(from)?;
        check_name(to)?;
        rfs::renameat(&self.fd, from, &self.fd, to).map_err(|e| sys_err("rename", e))
    }

    /// Removes the directory `name` and everything below it without following any link.
    pub fn remove_tree(&self, sys: &Sys, name: &str) -> Result<(), Refusal> {
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
