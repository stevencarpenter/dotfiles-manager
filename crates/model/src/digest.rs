//! Tree digest (docs/bootstrap.md).
//!
//! SHA-256 over entries sorted by path bytes. Each entry is
//! `<path> NUL <class> NUL <value> LF`, where class is `file`, `exec` or
//! `symlink` or `dir` and value is the content SHA-256 (hex), the symlink target, or empty for
//! a directory. A directory entry makes an empty directory part of the identity.

use sha2::{Digest, Sha256};

#[non_exhaustive]
pub enum EntryKind {
    File {
        exec: bool,
        sha256: String,
    },
    Symlink {
        target: String,
    },
    /// A real directory, recorded so an empty one is part of the identity.
    Dir,
}

pub struct TreeEntry {
    pub path: String,
    pub kind: EntryKind,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes).as_slice())
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

pub fn tree_digest(mut entries: Vec<TreeEntry>) -> String {
    entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let mut h = Sha256::new();
    for e in &entries {
        let (class, value) = match &e.kind {
            EntryKind::File {
                exec: false,
                sha256,
            } => ("file", sha256.as_str()),
            EntryKind::File { exec: true, sha256 } => ("exec", sha256.as_str()),
            EntryKind::Symlink { target } => ("symlink", target.as_str()),
            EntryKind::Dir => ("dir", ""),
        };
        h.update(e.path.as_bytes());
        h.update([0]);
        h.update(class.as_bytes());
        h.update([0]);
        h.update(value.as_bytes());
        h.update(b"\n");
    }
    hex(h.finalize().as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_order_independent_and_class_sensitive() {
        let f = |p: &str, exec| TreeEntry {
            path: p.into(),
            kind: EntryKind::File {
                exec,
                sha256: sha256_hex(p.as_bytes()),
            },
        };
        let a = tree_digest(vec![f("b", false), f("a", false)]);
        assert_eq!(a, tree_digest(vec![f("a", false), f("b", false)]));
        assert_ne!(a, tree_digest(vec![f("a", true), f("b", false)]));
    }
}
