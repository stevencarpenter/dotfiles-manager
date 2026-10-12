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
    Dir, Ident, Peek, Stamp, Sys, check_name, ident_of, kind, mode_bits, publish_err, rename_new,
    sys_err, temp_name, unconfirmed,
};
use dfm_model::plan::{file_fingerprint, link_fingerprint};
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
    /// The name must hold a regular file with exactly this content hash, owned by `uid`.
    Replace { expected_sha256: &'a str, uid: u32 },
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
    /// the new link, never neither. The live link must also be owned by `uid`. The checks and the
    /// rename are separate calls: a change between them is not detected (docs/apply.md, limits).
    #[doc(hidden)]
    pub fn replace_link(
        &self,
        name: &str,
        target: &str,
        ledger_fingerprint: &str,
        uid: u32,
        fail: &Fail,
    ) -> Result<(), Refusal> {
        check_name(name)?;
        self.require_owner(name, uid)?;
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

    /// Refuses unless the entry `name` exists and `uid` owns it (`fstatat`, no link followed), and
    /// returns the identity of the entry that was examined, so a caller that hashed an inode can
    /// tell that the owner check saw the same one. A publisher calls it beside its own content
    /// check, so the owner is checked where the entry is replaced or removed, not only when the
    /// operation started.
    pub(crate) fn require_owner(&self, name: &str, uid: u32) -> Result<Ident, Refusal> {
        check_name(name)?;
        match rfs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) if st.st_uid == uid => ident_of(&st),
            Ok(_) => Err(conflict(format!(
                "{} is owned by another user; it is left alone",
                self.at(name).display()
            ))),
            Err(Errno::NOENT) => Err(conflict(format!(
                "{} vanished while applying",
                self.at(name).display()
            ))),
            Err(e) => Err(sys_err("stat entry", e)),
        }
    }

    /// Removes the entry `name` like [`Dir::remove_entry`], after checking that `uid` owns it. The
    /// owner is checked beside the unlink, not only when the operation started.
    pub(crate) fn remove_owned(&self, name: &str, uid: u32, fail: &Fail) -> Result<(), Refusal> {
        self.require_owner(name, uid)?;
        self.remove_entry(name, fail)
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
                let found = rfs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW)
                    .ok()
                    .map(|st| kind(&st));
                Err(unlink_refusal(
                    &self.at(name).display().to_string(),
                    e,
                    found,
                ))
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
        Ok(Some((file_fingerprint(&buf), ident)))
    }

    /// Publishes `bytes` as the regular file `name` with `mode`: a same-directory staged file
    /// (`O_EXCL`, written, synced), then an exclusive rename (create) or `renameat` (replace). A failure
    /// before the commit point leaves the previous state and removes the staged file. A failure
    /// after it is `E_IO_UNCONFIRMED`. Failure points: `file-before-write`, `file-after-tmp`,
    /// `file-after-hash` (a replace, between the content check and the owner check), `file-after-rename`.
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
                    Err(e) => Err(publish_err(&self.at(name), e)),
                },
                Publish::Replace {
                    expected_sha256,
                    uid,
                } => {
                    let live = self.hash_file(name, limit)?;
                    fail.hit("file-after-hash")?;
                    let hashed = match &live {
                        Some((h, ident)) if h == expected_sha256 => *ident,
                        _ => {
                            return Err(conflict(format!(
                                "{} changed while applying; it is left alone",
                                self.at(name).display()
                            )));
                        }
                    };
                    // The owner check must see the inode that was hashed.
                    if self.require_owner(name, uid)? != hashed {
                        return Err(conflict(format!(
                            "{} was replaced while applying; it is left alone",
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

/// The refusal for an `unlinkat` that answered `ISDIR` or `PERM`. `found` is what a stat of the
/// entry says. A non-directory with `PERM` is an immutable flag (uchg) or a sticky directory.
fn unlink_refusal(shown: &str, e: Errno, found: Option<FileType>) -> Refusal {
    if e == Errno::ISDIR || found == Some(FileType::Directory) {
        conflict(format!(
            "{shown} is a directory; the manager never removes a directory"
        ))
    } else {
        sys_err(
            &format!(
                "remove {shown} (an immutable flag such as uchg, or a sticky directory owned by another user, forbids it; the entry is kept)"
            ),
            e,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn an_entry_of_another_owner_is_not_replaced() {
        let home = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("dfm-publish-owner-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("f"), "old").unwrap();
        let uid = std::fs::metadata(&home).unwrap().uid();
        let dir = Dir::open_home(&Sys { uid }, &home).unwrap();
        let hash = file_fingerprint(b"old");
        let replace = |uid| Publish::Replace {
            expected_sha256: &hash,
            uid,
        };
        let fail = Fail::none();
        let e = dir
            .publish_file("f", b"new", 0o600, replace(uid + 1), 1024, &fail)
            .unwrap_err();
        assert_eq!(e.code, Code::Conflict);
        assert_eq!(std::fs::read(home.join("f")).unwrap(), b"old");
        assert_eq!(
            dir.require_owner("f", uid + 1).unwrap_err().code,
            Code::Conflict
        );
        dir.require_owner("f", uid).unwrap();
        dir.publish_file("f", b"new", 0o600, replace(uid), 1024, &fail)
            .unwrap();
        assert_eq!(std::fs::read(home.join("f")).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn an_entry_of_another_owner_is_not_removed() {
        let home = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("dfm-publish-remove-owner-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("f"), "old").unwrap();
        let uid = std::fs::metadata(&home).unwrap().uid();
        let dir = Dir::open_home(&Sys { uid }, &home).unwrap();
        let e = dir.remove_owned("f", uid + 1, &Fail::none()).unwrap_err();
        assert_eq!(e.code, Code::Conflict);
        assert!(e.detail.contains("owned by another user"), "{}", e.detail);
        assert!(home.join("f").exists(), "the entry is kept");
        dir.remove_owned("f", uid, &Fail::none()).unwrap();
        assert!(!home.join("f").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn an_eperm_on_a_non_directory_is_a_permission_failure_not_a_directory() {
        for found in [Some(FileType::RegularFile), Some(FileType::Symlink), None] {
            let r = unlink_refusal("/h/a", Errno::PERM, found);
            assert_eq!(r.code, Code::IoFailed, "{found:?}");
            assert!(r.detail.contains("immutable flag"), "{}", r.detail);
            assert!(!r.detail.contains("is a directory"), "{}", r.detail);
        }
        for (e, found) in [
            (Errno::PERM, Some(FileType::Directory)),
            (Errno::ISDIR, None),
        ] {
            let r = unlink_refusal("/h/d", e, found);
            assert_eq!(r.code, Code::Conflict, "{e:?}");
            assert!(r.detail.contains("is a directory"), "{}", r.detail);
        }
    }
}
