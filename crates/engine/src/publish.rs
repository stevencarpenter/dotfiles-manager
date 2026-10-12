//! Mutating primitives for apply (docs/apply.md). Every one acts on a held, verified directory
//! descriptor with `*at` calls that never follow a link at the leaf, so a path component
//! swapped after the check cannot redirect the effect.
//!
//! Create operations never replace (`mkdirat`, `symlinkat` and `renameat` with `RENAME_EXCL` fail
//! with `EEXIST`).
//! Replacement is a same-directory temporary name and `renameat`. Removal is `unlinkat` without
//! `AT_REMOVEDIR`, so a directory is never removed and a link is removed, not followed.

use crate::fail::Fail;
use crate::fsys::{
    Dir, Peek, Stamp, Sys, check_name, kind, mode_bits, rename_new, sys_err, temp_name, unconfirmed,
};
use dfm_model::digest::sha256_hex;
use dfm_model::plan::link_fingerprint;
use dfm_model::{Code, Refusal};
use rustix::fs::{self as rfs, AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use std::fs::File;
use std::io::Read;

/// How a file is published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Publish<'a> {
    /// The name must be absent. The rename is exclusive (`RENAME_EXCL`), so a file that appeared
    /// since the check is a conflict.
    Create,
    /// The name must hold a regular file with exactly this content hash.
    Replace { expected_sha256: &'a str },
    /// The name must still be the file whose [`Stamp`] the caller captured when it read the live
    /// content: same device, inode, size, change time and modification time. This is the check
    /// for a co-owned file, whose whole-file hash is never recorded. It narrows the window to the
    /// gap between this check and `renameat`; it does not close it (docs/apply.md).
    ReplaceStamp(Stamp),
}

fn conflict(detail: impl Into<String>) -> Refusal {
    Refusal::new(Code::Conflict, detail)
}

/// Gives a link the mode 0755 a link normally has. `symlinkat` applies the umask on macOS, and a
/// link made under a restrictive umask (0500, say) cannot be read by its own owner. Linux links
/// have no mode, and its `fchmodat` refuses `AT_SYMLINK_NOFOLLOW`, which is accepted here.
fn normalize_link_mode(dir: &Dir, name: &str) -> Result<(), Refusal> {
    match rfs::chmodat(
        &dir.fd,
        name,
        Mode::from_raw_mode(0o755),
        AtFlags::SYMLINK_NOFOLLOW,
    ) {
        Ok(()) | Err(Errno::NOTSUP) => Ok(()),
        Err(e) => Err(sys_err("set link mode", e)),
    }
}

impl Dir {
    /// Opens `rel` below this directory one component at a time, each with `O_NOFOLLOW |
    /// O_DIRECTORY`, and requires every component to be owned by the current user. A link, a
    /// file, a missing component or another owner is `E_PARENT_UNSAFE`: nothing is followed,
    /// created or repaired. The result names the directory that was verified, whatever happens
    /// to its path afterwards.
    #[doc(hidden)]
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

    /// As [`Dir::open_below`], but `None` when a component does not exist. A link, a file or
    /// another owner is still `E_PARENT_UNSAFE`.
    #[doc(hidden)]
    pub fn open_below_opt(&self, sys: &Sys, rel: &[&str]) -> Result<Option<Dir>, Refusal> {
        let mut cur = self.try_clone()?;
        for name in rel {
            if matches!(cur.peek(sys, name)?, Peek::Missing) {
                return Ok(None);
            }
            cur = cur.open_below(sys, &[name])?;
        }
        Ok(Some(cur))
    }

    /// Creates the directory `name` with mode 0700 (the XDG default for a directory created on
    /// the user's behalf). An existing entry is a conflict and is left alone. The mode is set by
    /// name, without following a link, before the directory is opened, so a restrictive umask
    /// cannot leave a directory the process itself cannot open.
    #[doc(hidden)]
    pub fn make_dir(&self, name: &str, fail: &Fail) -> Result<(), Refusal> {
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
        // Committed. A failure from here on leaves the directory in place, so it is reported as
        // unconfirmed, not as a failure to create.
        unconfirmed(
            rfs::chmodat(
                &self.fd,
                name,
                Mode::from_raw_mode(0o700),
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(|e| sys_err("chmod new directory", e))
            .and_then(|()| fail.hit("dir-after-create"))
            .and_then(|()| self.sync()),
        )
    }

    /// Creates the link `name -> target`. `symlinkat` never replaces, so an entry that appeared
    /// since the check is a conflict.
    #[doc(hidden)]
    pub fn make_link(&self, name: &str, target: &str, fail: &Fail) -> Result<(), Refusal> {
        check_name(name)?;
        match rfs::symlinkat(target, &self.fd, name) {
            Ok(()) => unconfirmed(
                normalize_link_mode(self, name)
                    .and_then(|()| fail.hit("link-after-create"))
                    .and_then(|()| self.sync()),
            ),
            Err(Errno::EXIST) => Err(conflict(format!(
                "{} appeared while applying; it is left alone",
                self.at(name).display()
            ))),
            Err(e) => Err(sys_err(&format!("link {}", self.at(name).display()), e)),
        }
    }

    /// Replaces the link `name` with a link to `target`, only while the live link still has
    /// the fingerprint the ledger records (`link_fingerprint` of its target text). The new link
    /// is made under a hidden name and moved over with `renameat`, so a reader sees the old or
    /// the new link, never neither. The check and the rename are two calls: a change between
    /// them is not detected (docs/apply.md, limits).
    #[doc(hidden)]
    pub fn replace_link(
        &self,
        name: &str,
        target: &str,
        ledger_fingerprint: &str,
        fail: &Fail,
    ) -> Result<(), Refusal> {
        check_name(name)?;
        match rfs::readlinkat(&self.fd, name, Vec::new()) {
            Ok(t) if link_fingerprint(&t.to_string_lossy()) == ledger_fingerprint => {}
            Ok(_) => {
                return Err(conflict(format!(
                    "{} no longer matches the fingerprint the ledger records; it is left alone",
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
        let tmp = temp_name(name, "stage");
        rfs::symlinkat(target, &self.fd, &tmp).map_err(|e| sys_err("stage link", e))?;
        let moved = normalize_link_mode(self, &tmp)
            .and_then(|()| fail.hit("link-after-tmp"))
            .and_then(|()| {
                rfs::renameat(&self.fd, &tmp, &self.fd, name).map_err(|e| sys_err("rename", e))
            });
        if let Err(e) = moved {
            self.drop_temp(&tmp);
            return Err(e);
        }
        unconfirmed(fail.hit("link-after-rename").and_then(|()| self.sync()))
    }

    /// Removes the non-directory entry `name`. A link is removed, not followed. A directory is
    /// never removed: `unlinkat` without `AT_REMOVEDIR` refuses it.
    #[doc(hidden)]
    pub fn remove_entry(&self, name: &str, fail: &Fail) -> Result<(), Refusal> {
        check_name(name)?;
        fail.hit("remove-before-unlink")?;
        match rfs::unlinkat(&self.fd, name, AtFlags::empty()) {
            Ok(()) => unconfirmed(fail.hit("remove-after-unlink").and_then(|()| self.sync())),
            Err(Errno::NOENT) => Err(conflict(format!(
                "{} vanished while applying",
                self.at(name).display()
            ))),
            // macOS answers `unlink` of a directory with EPERM, as does an immutable flag (uchg)
            // or a sticky directory. Look at the entry to tell them apart.
            Err(e @ (Errno::ISDIR | Errno::PERM)) => {
                let dir = e == Errno::ISDIR
                    || rfs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW)
                        .is_ok_and(|st| kind(&st) == FileType::Directory);
                Err(if dir {
                    conflict(format!(
                        "{} is a directory; the manager never removes a directory",
                        self.at(name).display()
                    ))
                } else {
                    sys_err(
                        &format!(
                            "remove {} (an immutable flag such as uchg, or a sticky directory owned by another user, forbids it; the entry is kept)",
                            self.at(name).display()
                        ),
                        e,
                    )
                })
            }
            Err(e) => Err(sys_err(&format!("remove {}", self.at(name).display()), e)),
        }
    }

    /// Hashes the regular file `name` from one descriptor opened without following a link, and
    /// returns the hash with the descriptor's identity so a caller can check that the name still
    /// refers to the same file. `None` when the name is absent or is not a regular file.
    pub(crate) fn hash_file(
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
    /// (`O_EXCL`, written, synced), then an exclusive rename (create) or `renameat` (replace). A failure
    /// before the commit point leaves the previous state and removes the staged file. A failure
    /// after it is `E_IO_UNCONFIRMED`. Failure points: `file-before-write`, `file-after-tmp`,
    /// `file-after-rename`.
    pub(crate) fn publish_file(
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
                Publish::Create => match rename_new(&self.fd, &tmp, name) {
                    Ok(()) => Ok(()),
                    Err(Errno::EXIST) => Err(conflict(format!(
                        "{} appeared while applying; it is left alone",
                        self.at(name).display()
                    ))),
                    Err(e) => Err(sys_err("publish", e)),
                },
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
                Publish::ReplaceStamp(want) => {
                    if self.entry_stamp(name)? != Some(want) {
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
        unconfirmed(fail.hit("file-after-rename").and_then(|()| self.sync()))
    }

    /// The mode bits of the entry `name`, without following a link. `None` when absent.
    pub(crate) fn entry_mode(&self, name: &str) -> Result<Option<u32>, Refusal> {
        check_name(name)?;
        match rfs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => Ok(Some(mode_bits(&st))),
            Err(Errno::NOENT) => Ok(None),
            Err(e) => Err(sys_err("stat entry", e)),
        }
    }
}
