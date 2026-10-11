//! Mutating primitives for apply (docs/apply.md). Every one acts on a held, verified directory
//! descriptor with `*at` calls that never follow a link at the leaf, so a path component
//! swapped after the check cannot redirect the effect.
//!
//! Create operations never replace (`mkdirat`, `symlinkat` and `linkat` fail with `EEXIST`).
//! Replacement is a same-directory temporary name and `renameat`. Removal is `unlinkat` without
//! `AT_REMOVEDIR`, so a directory is never removed and a link is removed, not followed.

use crate::fail::Fail;
use crate::fsys::{Dir, Sys, check_name, kind, mode_bits, sys_err, unconfirmed};
use dfm_model::digest::sha256_hex;
use dfm_model::{Code, Refusal};
use rustix::fs::{self as rfs, AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use std::fs::File;
use std::io::Read;
use std::sync::atomic::{AtomicU32, Ordering};

/// How a file is published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Publish<'a> {
    /// The name must be absent. `linkat` never replaces, so a file that appeared since the
    /// check is a conflict.
    Create,
    /// The name must hold a regular file with exactly this content hash.
    Replace { expected_sha256: &'a str },
}

fn conflict(detail: impl Into<String>) -> Refusal {
    Refusal::new(Code::Conflict, detail)
}

/// A hidden same-directory name for a staged link or file.
fn temp_name(name: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!(
        ".{name}.{}.{}.{nanos}.stage",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

impl Dir {
    /// Opens `rel` below this directory one component at a time, each with `O_NOFOLLOW |
    /// O_DIRECTORY`, and requires every component to be owned by the current user. A link, a
    /// file, a missing component or another owner is `E_PARENT_UNSAFE`: nothing is followed,
    /// created or repaired. The result names the directory that was verified, whatever happens
    /// to its path afterwards.
    pub fn open_below(&self, sys: &Sys, rel: &[&str]) -> Result<Dir, Refusal> {
        let mut cur = self.try_clone()?;
        for name in rel {
            let unsafe_parent = |why: &str| {
                Refusal::new(
                    Code::ParentUnsafe,
                    format!("{} {why}", cur.at(name).display()),
                )
            };
            let next = cur.peek_dir(name)?.ok_or_else(|| {
                unsafe_parent("is not a real directory (missing, a link or a file)")
            })?;
            let st = rfs::fstat(&next.fd).map_err(|e| sys_err("stat directory", e))?;
            if st.st_uid != sys.uid {
                return Err(unsafe_parent("is owned by another user"));
            }
            cur = next;
        }
        Ok(cur)
    }

    /// Creates the directory `name` with mode 0700 (the XDG default for a directory created on
    /// the user's behalf). An existing entry is a conflict and is left alone.
    pub fn make_dir(&self, name: &str) -> Result<(), Refusal> {
        check_name(name)?;
        match rfs::mkdirat(&self.fd, name, Mode::from_raw_mode(0o700)) {
            Ok(()) => {}
            Err(Errno::EXIST) => {
                return Err(conflict(format!(
                    "{} appeared while applying; it is left alone",
                    self.at(name).display()
                )));
            }
            Err(e) => return Err(sys_err(&format!("create {}", self.at(name).display()), e)),
        }
        // `mkdirat` applies the umask. The mode is stated, not inherited.
        let fd = rfs::openat(
            &self.fd,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| sys_err("open new directory", e))?;
        rfs::fchmod(&fd, Mode::from_raw_mode(0o700)).map_err(|e| sys_err("chmod directory", e))?;
        self.sync()
    }

    /// Creates the link `name -> target`. `symlinkat` never replaces, so an entry that appeared
    /// since the check is a conflict.
    pub fn make_link(&self, name: &str, target: &str) -> Result<(), Refusal> {
        check_name(name)?;
        match rfs::symlinkat(target, &self.fd, name) {
            Ok(()) => self.sync(),
            Err(Errno::EXIST) => Err(conflict(format!(
                "{} appeared while applying; it is left alone",
                self.at(name).display()
            ))),
            Err(e) => Err(sys_err(&format!("link {}", self.at(name).display()), e)),
        }
    }

    /// Replaces the link `name` with a link to `target`, only while it still points at
    /// `expected_target`. The new link is made under a hidden name and moved over with
    /// `renameat`, so a reader sees the old or the new link, never neither. The check and the
    /// rename are two calls: a change between them is not detected (docs/apply.md, limits).
    pub fn replace_link(
        &self,
        name: &str,
        target: &str,
        expected_target: &str,
        fail: &Fail,
    ) -> Result<(), Refusal> {
        check_name(name)?;
        match rfs::readlinkat(&self.fd, name, Vec::new()) {
            Ok(t) if t.to_string_lossy() == expected_target => {}
            Ok(_) => {
                return Err(conflict(format!(
                    "{} changed while applying",
                    self.at(name).display()
                )));
            }
            Err(Errno::INVAL) => {
                return Err(conflict(format!(
                    "{} is no longer a link; it is left alone",
                    self.at(name).display()
                )));
            }
            Err(e) => return Err(sys_err("read link", e)),
        }
        let tmp = temp_name(name);
        rfs::symlinkat(target, &self.fd, &tmp).map_err(|e| sys_err("stage link", e))?;
        let moved = fail.hit("link-after-tmp").and_then(|()| {
            rfs::renameat(&self.fd, &tmp, &self.fd, name).map_err(|e| sys_err("rename", e))
        });
        if let Err(e) = moved {
            self.drop_temp(&tmp);
            return Err(e);
        }
        unconfirmed(self.sync())
    }

    /// Removes the non-directory entry `name`. A link is removed, not followed. A directory is
    /// never removed: `unlinkat` without `AT_REMOVEDIR` refuses it.
    pub fn remove_entry(&self, name: &str) -> Result<(), Refusal> {
        check_name(name)?;
        match rfs::unlinkat(&self.fd, name, AtFlags::empty()) {
            Ok(()) => unconfirmed(self.sync()),
            Err(Errno::NOENT) => Err(conflict(format!(
                "{} vanished while applying",
                self.at(name).display()
            ))),
            Err(Errno::ISDIR | Errno::PERM) => Err(conflict(format!(
                "{} is a directory; the manager never removes a directory",
                self.at(name).display()
            ))),
            Err(e) => Err(sys_err(&format!("remove {}", self.at(name).display()), e)),
        }
    }

    /// Hashes the regular file `name` from one descriptor opened without following a link, and
    /// returns the hash with the descriptor's identity so a caller can check that the name still
    /// refers to the same file. `None` when the name is absent or is not a regular file.
    pub fn hash_file(
        &self,
        name: &str,
        limit: u64,
    ) -> Result<Option<(String, crate::fsys::Ident)>, Refusal> {
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
        let ident = crate::fsys::ident_of(&st)?;
        let mut buf = Vec::new();
        File::from(fd)
            .take(limit.saturating_add(1))
            .read_to_end(&mut buf)
            .map_err(|e| crate::fsys::io("read file", &e))?;
        if u64::try_from(buf.len()).unwrap_or(u64::MAX) > limit {
            return Ok(None);
        }
        Ok(Some((sha256_hex(&buf), ident)))
    }

    /// Publishes `bytes` as the regular file `name` with `mode`: a same-directory staged file
    /// (`O_EXCL`, written, synced), then `linkat` (create) or `renameat` (replace). A failure
    /// before the commit point leaves the previous state and removes the staged file. A failure
    /// after it is `E_IO_UNCONFIRMED`. Failure points: `file-before-write`, `file-after-tmp`,
    /// `file-after-rename`.
    pub fn publish_file(
        &self,
        name: &str,
        bytes: &[u8],
        mode: u32,
        how: Publish<'_>,
        limit: u64,
        fail: &Fail,
    ) -> Result<(), Refusal> {
        check_name(name)?;
        if mode > 0o777 {
            return Err(conflict("a file mode above 0777 is not published"));
        }
        fail.hit("file-before-write")?;
        let tmp = self.write_temp_mode(name, bytes, mode)?;
        let commit = || -> Result<(), Refusal> {
            fail.hit("file-after-tmp")?;
            match how {
                Publish::Create => {
                    match rfs::linkat(&self.fd, &tmp, &self.fd, name, AtFlags::empty()) {
                        Ok(()) => Ok(()),
                        Err(Errno::EXIST) => Err(conflict(format!(
                            "{} appeared while applying; it is left alone",
                            self.at(name).display()
                        ))),
                        Err(e) => Err(sys_err("publish", e)),
                    }
                }
                Publish::Replace { expected_sha256 } => {
                    let live = self.hash_file(name, limit)?;
                    if live.as_ref().map(|(h, _)| h.as_str()) != Some(expected_sha256) {
                        return Err(conflict(format!(
                            "{} changed while applying; it is left alone",
                            self.at(name).display()
                        )));
                    }
                    rfs::renameat(&self.fd, &tmp, &self.fd, name).map_err(|e| sys_err("rename", e))
                }
            }
        };
        if let Err(e) = commit() {
            self.drop_temp(&tmp);
            return Err(e);
        }
        // Committed. A failure from here on only leaves durability (or a stray name) unknown.
        let after = || -> Result<(), Refusal> {
            if matches!(how, Publish::Create) {
                rfs::unlinkat(&self.fd, &tmp, AtFlags::empty())
                    .map_err(|e| sys_err("remove staged name", e))?;
            }
            fail.hit("file-after-rename")?;
            self.sync()
        };
        unconfirmed(after())
    }

    /// The mode bits of the entry `name`, without following a link. `None` when absent.
    pub fn entry_mode(&self, name: &str) -> Result<Option<u32>, Refusal> {
        check_name(name)?;
        match rfs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => Ok(Some(mode_bits(&st))),
            Err(Errno::NOENT) => Ok(None),
            Err(e) => Err(sys_err("stat entry", e)),
        }
    }
}
