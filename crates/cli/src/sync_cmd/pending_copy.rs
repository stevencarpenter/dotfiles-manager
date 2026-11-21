//! An interrupted cross-volume publication leaves its copy on the destination volume.
//! Bind its exact directory to the current checkout before copying any source bytes.

use super::{conflict, io_failed};
use dfm_engine::{Dir, Fail, Sys, now_ns};
use dfm_model::{Refusal, digest::sha256_hex};
use std::fs;
use std::path::{Component, Path, PathBuf};

pub(super) struct PendingCopy<'a> {
    cache: Dir,
    record: String,
    root: &'a Path,
    sys: Sys,
}

pub(super) struct Found {
    pub path: PathBuf,
    directory: Dir,
    token: String,
    token_dir: Dir,
    checkout: Option<Dir>,
}

impl<'a> PendingCopy<'a> {
    pub fn new(cache: &Path, root: &'a Path, sys: Sys) -> Result<Self, Refusal> {
        let cache = Dir::open_plain(cache)?;
        cache.recheck(&sys, true)?;
        let key = sha256_hex(root.as_os_str().as_encoded_bytes());
        Ok(Self {
            cache,
            record: format!("sync-copy-{key}.json"),
            root,
            sys,
        })
    }

    pub fn register(&self, checkout: &Path) -> Result<(), Refusal> {
        let stage = checkout
            .parent()
            .ok_or_else(|| conflict("the checkout copy has no parent"))?;
        let rel = stage
            .strip_prefix(self.root)
            .map_err(|_| conflict("the checkout copy is outside the root"))?;
        let io = |e| io_failed("register the checkout copy", &e);
        let dir = Dir::open_plain(stage)?;
        let child = Dir::open_plain(checkout)?;
        let token = format!("owner-{}", now_ns()?);
        fs::create_dir(stage.join(&token)).map_err(io)?;
        let record = (
            rel,
            dir.ident()?.device_inode(),
            child.ident()?.device_inode(),
            token,
        );
        let bytes = serde_json::to_vec(&record).map_err(|e| conflict(e.to_string()))?;
        self.cache
            .replace_file(&self.record, &bytes, &Fail::none(), "sync-copy")
    }

    pub fn find(&self) -> Result<Option<Found>, Refusal> {
        let Some(bytes) = self.cache.read_private(&self.sys, &self.record, 16384)? else {
            return Ok(None);
        };
        let (rel, parent_id, child_id, token): (PathBuf, (u64, u64), (u64, u64), String) =
            serde_json::from_slice(&bytes)
                .map_err(|e| conflict(format!("invalid checkout copy record: {e}")))?;
        let mut dir = Dir::open_plain(self.root)?;
        for part in rel.components() {
            let Component::Normal(name) = part else {
                return Err(conflict("invalid checkout copy path"));
            };
            let name = name
                .to_str()
                .ok_or_else(|| conflict("the checkout copy path is not UTF-8"))?;
            let Some(next) = dir.peek_dir(name)? else {
                return Ok(None);
            };
            dir = next;
        }
        let path = self.root.join(rel);
        if dir.ident()?.device_inode() != parent_id {
            return Ok(None);
        }
        let Some(token_dir) = dir.peek_dir(&token)? else {
            return Ok(None);
        };
        let checkout = dir.peek_dir("checkout")?;
        if let Some(child) = &checkout {
            if child.ident()?.device_inode() != child_id {
                return Ok(None);
            }
        } else if dir.entry_ident("checkout")?.is_some() {
            return Ok(None);
        }
        let listing = dir.list_upto(2)?;
        if listing.odd != 0
            || listing.names.iter().any(|n| n != &token && n != "checkout")
            || token_dir.list_upto(0)?.seen() != 0
        {
            return Ok(None);
        }
        dir.recheck(&self.sys, true)?;
        Ok(Some(Found {
            path,
            directory: dir,
            token,
            token_dir,
            checkout,
        }))
    }

    /// The caller holds the writer lock. Remove children through the held descriptor so a
    /// replacement at the recorded path never supplies the tree to be removed.
    pub fn recover(&self, found: Found) -> Result<(), Refusal> {
        let parent = found
            .path
            .parent()
            .ok_or_else(|| conflict("the checkout copy has no parent"))?;
        let name = found
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| conflict("invalid checkout copy name"))?;
        let parent = Dir::open_plain(parent)?;
        if parent.entry_ident(name)? != Some(found.directory.ident()?) {
            return Err(conflict("the checkout copy directory was replaced"));
        }
        if let Some(child) = found.checkout {
            found
                .directory
                .remove_open_tree(&self.sys, "checkout", &child)?;
        }
        found
            .directory
            .remove_open_tree(&self.sys, &found.token, &found.token_dir)?;
        if parent.entry_ident(name)? == Some(found.directory.ident()?) {
            match fs::remove_dir(&found.path) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => (),
                Err(e) => return Err(io_failed("remove the checkout copy directory", &e)),
            }
        }
        self.forget()
    }

    pub fn forget(&self) -> Result<(), Refusal> {
        fs::remove_file(self.cache.shown().join(&self.record))
            .map_err(|e| io_failed("remove the checkout copy record", &e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn a_replaced_stage_or_child_is_never_claimed_by_its_name() {
        let base = std::env::temp_dir().join(format!(
            "dfm-copy-retry-{}-{}",
            std::process::id(),
            now_ns().unwrap()
        ));
        let root = base.join("root");
        let cache = base.join("cache");
        let stage = root.join("stage");
        let checkout = stage.join("checkout");
        fs::create_dir_all(&checkout).unwrap();
        fs::create_dir(&cache).unwrap();
        for dir in [&cache, &stage] {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let sys = Sys {
            uid: fs::metadata(&base).unwrap().uid(),
        };
        let copies = PendingCopy::new(&cache, &root, sys).unwrap();
        copies.register(&checkout).unwrap();
        let held = copies.find().unwrap().unwrap();
        let moved = base.join("original");
        fs::rename(&checkout, &moved).unwrap();
        fs::create_dir(&checkout).unwrap();
        fs::write(checkout.join("mine"), "user").unwrap();
        assert!(copies.find().unwrap().is_none());
        assert!(copies.recover(held).is_err());
        assert_eq!(fs::read_to_string(checkout.join("mine")).unwrap(), "user");
        fs::rename(&stage, base.join("old-stage")).unwrap();
        fs::create_dir(&stage).unwrap();
        assert!(copies.find().unwrap().is_none());
        fs::remove_dir_all(base).unwrap();
    }
}
