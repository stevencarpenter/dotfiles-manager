//! Storage topology, the writer lock and atomic file operations (docs/state.md).
//!
//! `unsafe` is forbidden in this workspace and `std` has no `openat`, so these checks walk each
//! path component with `lstat` and open files with `O_NOFOLLOW`. A path component swapped between
//! a check and the use that follows is a residual window that a descriptor-relative
//! implementation (docs/architecture.md section 8, "Filesystem writes") would close. It needs
//! `libc` or `rustix`, which this task does not add.

use crate::fail::Fail;
use dfm_model::{Code, Refusal};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Platform facts the engine must not hard-code: the effective user and the open flags.
#[derive(Clone, Copy, Debug)]
pub struct Sys {
    pub uid: u32,
    pub o_nofollow: i32,
    pub o_nonblock: i32,
    pub eloop: i32,
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
    Refusal::new(Code::IoFailed, format!("{what}: {}", e.kind()))
}

/// What a directory check found.
#[derive(Debug, PartialEq, Eq)]
pub enum Found {
    Present,
    Absent,
}

/// HOME resolved through every alias (for example `/var` to `/private/var` on macOS). HOME must
/// be a directory owned by the current user and not writable by group or others.
pub fn canonical_home(sys: &Sys, home: &Path) -> Result<PathBuf, Refusal> {
    let real =
        fs::canonicalize(home).map_err(|e| storage(format!("{}: {}", home.display(), e.kind())))?;
    let m = fs::metadata(&real).map_err(|e| io("stat home", &e))?;
    if !m.is_dir() || m.uid() != sys.uid || m.mode() & 0o022 != 0 {
        return Err(storage(format!(
            "{} is not a directory owned by the current user and closed to group and others",
            real.display()
        )));
    }
    Ok(real)
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

/// Walks `rel` below `base`. Each component must be a real directory (never a symlink, never
/// another file type) owned by the current user. Components from index `private_from` on must be
/// exactly 0700; earlier ones (shared parents such as `.config`) only closed to group and
/// others. A missing component is created with mode 0700 when `create` is set, otherwise the
/// walk stops with `Absent`. Nothing is ever repaired or replaced.
pub fn check_dir_chain(
    sys: &Sys,
    base: &Path,
    rel: &[String],
    private_from: usize,
    create: bool,
) -> Result<Found, Refusal> {
    let mut p = base.to_path_buf();
    for (i, c) in rel.iter().enumerate() {
        p.push(c);
        let m = match fs::symlink_metadata(&p) {
            Ok(m) => m,
            Err(e) if e.kind() == ErrorKind::NotFound && create => {
                match fs::DirBuilder::new().mode(0o700).create(&p) {
                    Ok(()) => {}
                    // A concurrent creator is fine; the check below validates what exists.
                    Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(io(&format!("create {}", p.display()), &e)),
                }
                fs::set_permissions(&p, fs::Permissions::from_mode(0o700))
                    .map_err(|e| io(&format!("chmod {}", p.display()), &e))?;
                // The new directory entry must be durable before anything is written inside.
                if let Some(parent) = p.parent() {
                    sync_dir(parent)?;
                }
                fs::symlink_metadata(&p).map_err(|e| io("stat", &e))?
            }
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Found::Absent),
            Err(e) => return Err(storage(format!("{}: {}", p.display(), e.kind()))),
        };
        if m.file_type().is_symlink() {
            return Err(storage(format!(
                "{} is a symlink; a redirected manager path is refused, not repaired",
                p.display()
            )));
        }
        if !m.is_dir() {
            return Err(storage(format!(
                "{} is not a directory; a conflicting path type is refused, not replaced",
                p.display()
            )));
        }
        if m.uid() != sys.uid {
            return Err(storage(format!(
                "{} is not owned by the current user",
                p.display()
            )));
        }
        let mode = m.mode() & 0o777;
        let ok = if i >= private_from {
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
    }
    Ok(Found::Present)
}

/// Reads a manager-owned file: no symlink, regular, owned by the current user, mode 0600, at
/// most `limit` bytes. `None` when it does not exist.
pub fn read_private(sys: &Sys, path: &Path, limit: usize) -> Result<Option<Vec<u8>>, Refusal> {
    let bad = |d: String| storage(format!("{}: {d}", path.display()));
    let f = match OpenOptions::new()
        .read(true)
        .custom_flags(sys.o_nofollow | sys.o_nonblock)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(sys.eloop) => return Err(bad("is a symlink".into())),
        Err(e) => return Err(bad(e.kind().to_string())),
    };
    let m = f.metadata().map_err(|e| bad(e.kind().to_string()))?;
    if !m.is_file() {
        return Err(bad("not a regular file".into()));
    }
    if m.uid() != sys.uid {
        return Err(bad("not owned by the current user".into()));
    }
    if m.mode() & 0o777 != 0o600 {
        return Err(bad(format!(
            "mode is {:o}, expected 0600",
            m.mode() & 0o777
        )));
    }
    let mut buf = Vec::new();
    f.take(limit as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| bad(e.kind().to_string()))?;
    if buf.len() > limit {
        return Err(Refusal::new(
            Code::LimitExceeded,
            format!("{} exceeds {limit} bytes", path.display()),
        ));
    }
    Ok(Some(buf))
}

pub fn sync_dir(dir: &Path) -> Result<(), Refusal> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| io("sync directory", &e))
}

/// A same-directory temporary file, fully written and synced, named so that readers ignore it.
fn write_temp(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, Refusal> {
    static N: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let tmp = dir.join(format!(
        ".{name}.{}.{}.{nanos}.tmp",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| {
            // The umask can clear bits of the creation mode; state files are exactly 0600.
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
            f.write_all(bytes)?;
            f.sync_all()
        });
    match written {
        Ok(()) => Ok(tmp),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(io("write temporary file", &e))
        }
    }
}

fn require_plain(target: &Path) -> Result<(), Refusal> {
    match fs::symlink_metadata(target) {
        Ok(m) if m.is_file() => Ok(()),
        Ok(_) => Err(storage(format!(
            "{} exists and is not a regular file",
            target.display()
        ))),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io("stat target", &e)),
    }
}

/// Atomic replacement: temporary file, fsync, `rename`, directory fsync. Any failure before the
/// rename leaves the previous file untouched, which is the last good state. A failure after it
/// is `E_IO_UNCONFIRMED`: the new file is in place and only its durability is unknown. Failure points are
/// `<label>-before-write`, `<label>-after-tmp` and `<label>-after-rename`.
pub fn replace_file(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    fail: &Fail,
    label: &str,
) -> Result<(), Refusal> {
    let target = dir.join(name);
    require_plain(&target)?;
    fail.hit(&format!("{label}-before-write"))?;
    let tmp = write_temp(dir, name, bytes)?;
    let step = || -> Result<(), Refusal> {
        fail.hit(&format!("{label}-after-tmp"))?;
        fs::rename(&tmp, &target).map_err(|e| io("rename", &e))
    };
    if let Err(e) = step() {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    // The rename happened. From here a failure means the new file is in place but its durability
    // is unconfirmed, which is a different outcome from a failure that left the old file.
    unconfirmed(
        fail.hit(&format!("{label}-after-rename"))
            .and_then(|()| sync_dir(dir)),
    )
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

/// Create-only publication (`hard_link` never replaces). `Ok(false)` means the name existed and
/// nothing was written. Failure points are as for [`replace_file`], with `-after-tmp` before the
/// link.
pub fn publish_new(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    fail: &Fail,
    label: &str,
) -> Result<bool, Refusal> {
    fail.hit(&format!("{label}-before-write"))?;
    let tmp = write_temp(dir, name, bytes)?;
    let target = dir.join(name);
    let step = || -> Result<bool, Refusal> {
        fail.hit(&format!("{label}-after-tmp"))?;
        match fs::hard_link(&tmp, &target) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(io("publish", &e)),
        }
    };
    let r = step();
    let _ = fs::remove_file(&tmp);
    if r.as_ref().is_ok_and(|created| *created) {
        unconfirmed(
            fail.hit(&format!("{label}-after-rename"))
                .and_then(|()| sync_dir(dir)),
        )?;
    }
    r
}

/// Appends one complete line. The file must exist, be private and end with a newline (or be
/// empty), so a torn earlier line is never extended. Failure points: `<label>-before-write` and
/// `<label>-after-write`.
pub fn append_line(
    sys: &Sys,
    path: &Path,
    line: &[u8],
    fail: &Fail,
    label: &str,
) -> Result<(), Refusal> {
    use std::io::{Seek, SeekFrom};
    let bad = |d: String| storage(format!("{}: {d}", path.display()));
    let mut f = OpenOptions::new()
        .read(true)
        .append(true)
        .custom_flags(sys.o_nofollow)
        .open(path)
        .map_err(|e| {
            if e.raw_os_error() == Some(sys.eloop) {
                bad("is a symlink".into())
            } else {
                bad(e.kind().to_string())
            }
        })?;
    let m = f.metadata().map_err(|e| io("stat", &e))?;
    if !m.is_file() || m.uid() != sys.uid || m.mode() & 0o777 != 0o600 || m.nlink() != 1 {
        return Err(bad(
            "must be a private regular file with one link owned by the current user".into(),
        ));
    }
    if m.len() > 0 {
        let mut last = [0u8; 1];
        f.seek(SeekFrom::End(-1))
            .and_then(|_| f.read_exact(&mut last))
            .map_err(|e| io("read journal tail", &e))?;
        if last[0] != b'\n' {
            return Err(Refusal::new(
                Code::StateInvalid,
                format!(
                    "{} ends with a torn line; it is reported, not extended",
                    path.display()
                ),
            ));
        }
    }
    fail.hit(&format!("{label}-before-write"))?;
    f.write_all(line)
        .and_then(|()| f.sync_all())
        .map_err(|e| io("append", &e))?;
    fail.hit(&format!("{label}-after-write"))
}

/// The advisory writer lock (`flock`). The kernel releases it when the process ends, so an
/// abrupt exit cannot leave it held. Dropping the value releases it too.
///
/// A lock on a deleted or replaced file protects nothing, so the holder compares the file it
/// locked with the path before each write ([`Lock::verify`]).
#[derive(Debug)]
pub struct Lock {
    _file: File,
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl Lock {
    /// Takes the exclusive lock without waiting. A second holder gets `E_LOCKED`.
    pub fn acquire(sys: &Sys, path: &Path) -> Result<Lock, Refusal> {
        let bad = |d: String| storage(format!("{}: {d}", path.display()));
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            // O_NONBLOCK so a FIFO planted at this path cannot hang the open.
            .custom_flags(sys.o_nofollow | sys.o_nonblock)
            .open(path)
            .map_err(|e| {
                if e.raw_os_error() == Some(sys.eloop) {
                    bad("is a symlink".into())
                } else {
                    bad(e.kind().to_string())
                }
            })?;
        let m = f.metadata().map_err(|e| io("stat lock", &e))?;
        if !m.is_file() || m.uid() != sys.uid {
            return Err(bad(
                "must be a regular file owned by the current user".into()
            ));
        }
        let mode = m.mode() & 0o777;
        if mode != 0o600 {
            // A umask can clear bits of the creation mode. Tightening to 0600 is allowed, a
            // looser mode is refused.
            if mode & !0o600 != 0 {
                return Err(bad(format!("mode is {mode:o}, expected 0600")));
            }
            f.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|e| io("chmod lock", &e))?;
        }
        match f.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let mut holder = String::new();
                let _ = f.read_to_string(&mut holder);
                return Err(Refusal::new(
                    Code::Locked,
                    format!("another command holds the manager lock ({})", holder.trim()),
                ));
            }
            Err(TryLockError::Error(e)) => return Err(io("lock", &e)),
        }
        let lock = Lock {
            _file: f,
            path: path.to_path_buf(),
            dev: m.dev(),
            ino: m.ino(),
        };
        lock.verify()?;
        // Informational only: the lock is the flock, not these bytes.
        let since = crate::store::now_unix().map_or_else(|_| "unknown".into(), |t| t.to_string());
        let note = format!("pid {} since {since}\n", std::process::id());
        let mut f = &lock._file;
        f.set_len(0)
            .and_then(|()| f.write_all(note.as_bytes()))
            .map_err(|e| io("write lock note", &e))?;
        Ok(lock)
    }

    /// Fails unless the path still names the file this lock holds. Called before every write.
    pub fn verify(&self) -> Result<(), Refusal> {
        let lost = |d: &str| {
            Refusal::new(
                Code::Locked,
                format!("{}: {d}; this writer must stop", self.path.display()),
            )
        };
        match fs::symlink_metadata(&self.path) {
            Ok(m) if m.is_file() && m.dev() == self.dev && m.ino() == self.ino => Ok(()),
            Ok(_) => Err(lost("the lock file was replaced")),
            Err(_) => Err(lost("the lock file was removed")),
        }
    }
}
