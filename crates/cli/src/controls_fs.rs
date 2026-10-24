//! Reads a staged tree and the toolchain tree from disk and applies the build
//! controls in `dfm_model::controls` (docs/architecture.md section 8, item 3a).
//! Read-only. Starts no process except `id -u`, which names the user that must own the toolchain
//! and every directory above it below HOME.

use crate::read_fs::sha256_file;
use dfm_model::controls::{self, MAX_FILE_BYTES};
use dfm_model::digest::{EntryKind, TreeEntry, tree_digest};
use dfm_model::{Code, Refusal};
use dfm_platform_macos::Platform;
use std::fs;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

const LOCK_FILE: &str = "dotfiles-manager.lock";

enum Node {
    File,
    Link(String),
}

/// Lists regular files and symlinks below `root`, relative and sorted by bytes.
/// Any other entry type or a name outside printable ASCII is an `Err(detail)`.
fn walk(root: &Path) -> Result<Vec<(String, Node)>, String> {
    fn go(dir: &Path, rel: &str, out: &mut Vec<(String, Node)>) -> Result<(), String> {
        let rd = fs::read_dir(dir).map_err(|e| format!("cannot read {rel}: {}", e.kind()))?;
        for entry in rd {
            let entry = entry.map_err(|e| format!("cannot read {rel}: {}", e.kind()))?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| format!("non-UTF-8 name in {rel}"))?;
            if !name
                .bytes()
                .all(|b| (0x20..=0x7e).contains(&b) && b != b'\\')
            {
                return Err(format!("unsupported file name in {rel}"));
            }
            let path = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let ty = entry
                .file_type()
                .map_err(|e| format!("cannot stat {path}: {}", e.kind()))?;
            if ty.is_symlink() {
                let t = fs::read_link(entry.path())
                    .map_err(|e| format!("cannot read link {path}: {}", e.kind()))?;
                let t = t
                    .into_os_string()
                    .into_string()
                    .map_err(|_| format!("non-UTF-8 link {path}"))?;
                out.push((path, Node::Link(t)));
            } else if ty.is_dir() {
                go(&entry.path(), &path, out)?;
            } else if ty.is_file() {
                out.push((path, Node::File));
            } else {
                return Err(format!("special file {path}"));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    go(root, "", &mut out)?;
    out.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    Ok(out)
}

fn read_bounded(path: &Path, rel: &str) -> Result<String, Refusal> {
    let untrusted = |d: String| Refusal::new(Code::SourceUntrusted, d);
    let mut f =
        fs::File::open(path).map_err(|e| untrusted(format!("cannot open {rel}: {}", e.kind())))?;
    let mut buf = Vec::new();
    f.by_ref()
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| untrusted(format!("cannot read {rel}: {}", e.kind())))?;
    if buf.len() as u64 > MAX_FILE_BYTES {
        return Err(untrusted(format!("{rel} exceeds 1 MiB")));
    }
    String::from_utf8(buf).map_err(|_| untrusted(format!("{rel} is not UTF-8")))
}

fn staged_tree(stage: &Path) -> Result<(String, String), Refusal> {
    let untrusted = |d: String| Refusal::new(Code::SourceUntrusted, d);
    let meta =
        fs::symlink_metadata(stage).map_err(|e| untrusted(format!("staged tree: {}", e.kind())))?;
    if !meta.is_dir() {
        return Err(untrusted("staged tree is not a directory".into()));
    }
    let nodes = walk(stage).map_err(untrusted)?;
    if let Some((p, _)) = nodes.iter().find(|(_, n)| matches!(n, Node::Link(_))) {
        return Err(untrusted(format!("symlink in staged tree: {p}")));
    }
    if !nodes.iter().any(|(p, _)| p == LOCK_FILE) {
        return Err(untrusted(format!("{LOCK_FILE} missing")));
    }
    let (pin, tree) =
        controls::parse_source_lock(&read_bounded(&stage.join(LOCK_FILE), LOCK_FILE)?)
            .map_err(|e| untrusted(format!("{LOCK_FILE}: {e}")))?;
    if !nodes.iter().any(|(p, _)| p == "Cargo.lock") {
        return Err(untrusted("Cargo.lock missing".into()));
    }
    controls::check_cargo_lock(&read_bounded(&stage.join("Cargo.lock"), "Cargo.lock")?)
        .map_err(|e| untrusted(format!("Cargo.lock: {e}")))?;
    if let Some(p) = controls::case_collision(nodes.iter().map(|(p, _)| p.as_str())) {
        return Err(untrusted(format!(
            "{p}: differs from another path only in case"
        )));
    }
    let mut root_workspace = false;
    for (rel, _) in &nodes {
        // Names are matched case-insensitively because the file system is.
        let lower = rel.to_ascii_lowercase();
        let base = lower.rsplit('/').next().unwrap_or(&lower);
        let in_cargo_dir = lower.rsplit('/').nth(1) == Some(".cargo");
        if in_cargo_dir && (base == "config.toml" || base == "config") {
            return Err(untrusted(format!(
                "{rel}: cargo configuration in staged tree"
            )));
        }
        match base {
            "rust-toolchain" => {
                return Err(untrusted(format!("{rel}: legacy rust-toolchain file")));
            }
            "rust-toolchain.toml" => {
                controls::check_toolchain_file(&read_bounded(&stage.join(rel), rel)?, &pin)
                    .map_err(|e| untrusted(format!("{rel}: {e}")))?
            }
            "cargo.toml" => {
                let depth = rel.matches('/').count();
                let text = read_bounded(&stage.join(rel), rel)?;
                controls::check_manifest(&text, depth)
                    .map_err(|e| untrusted(format!("{rel}: {e}")))?;
                root_workspace |= depth == 0 && controls::declares_workspace(&text);
            }
            _ => {}
        }
    }
    if !root_workspace {
        return Err(untrusted(
            "the root Cargo.toml must declare [workspace]".into(),
        ));
    }
    Ok((pin, tree))
}

/// One toolchain entry must belong to `uid` and, unless it is a symlink (whose mode is not used),
/// must not be writable by group or others.
pub(crate) fn closed_entry(md: &fs::Metadata, name: &str, uid: u32) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if md.uid() != uid {
        return Err(format!("{name} is not owned by the current user"));
    }
    if !md.file_type().is_symlink() && md.permissions().mode() & 0o022 != 0 {
        return Err(format!("{name} is writable by group or others"));
    }
    Ok(())
}

/// Refuses a toolchain tree that another user owns or that group or others can modify. The digest
/// records only the owner-execute bit, and an entry with another owner could be rewritten after the
/// digest. `uid` is the one owner allowed. The script's `toolchain_digest` applies the same rules.
fn closed_tree(dir: &Path, rel: &str, uid: u32) -> Result<(), String> {
    let ioerr = |e: std::io::Error| format!("{rel}: {}", e.kind());
    closed_entry(&fs::symlink_metadata(dir).map_err(ioerr)?, rel, uid)?;
    for e in fs::read_dir(dir).map_err(ioerr)? {
        let e = e.map_err(ioerr)?;
        let name = format!("{rel}/{}", e.file_name().to_string_lossy());
        if e.file_type().map_err(ioerr)?.is_dir() {
            closed_tree(&e.path(), &name, uid)?;
        } else {
            // DirEntry::metadata does not follow a symlink, so a link is checked for its own owner.
            closed_entry(&e.metadata().map_err(ioerr)?, &name, uid)?;
        }
    }
    Ok(())
}

/// Refuses a toolchain location whose existing ancestors below HOME are a link, not a directory,
/// owned by another user or open to group or others: such a user could swap an ancestor for a
/// link after the digest. The rules of `check_private_path` in the script. A missing ancestor is
/// fine here (the caller refuses an absent toolchain on its own).
pub(crate) fn closed_ancestors(home: &Path, dir: &Path, uid: u32) -> Result<(), String> {
    let rel = dir
        .parent()
        .and_then(|p| p.strip_prefix(home).ok())
        .ok_or_else(|| format!("{} is not below HOME", dir.display()))?;
    let mut cur = home.to_path_buf();
    for c in rel.components() {
        cur.push(c);
        let md = match fs::symlink_metadata(&cur) {
            Ok(md) => md,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("cannot stat {}: {}", cur.display(), e.kind())),
        };
        if md.file_type().is_symlink() {
            return Err(format!("{} is a symlink", cur.display()));
        }
        if !md.is_dir() {
            return Err(format!("{} is not a directory", cur.display()));
        }
        closed_entry(&md, &cur.display().to_string(), uid)?;
    }
    Ok(())
}

/// Digest of an installed toolchain tree (docs/architecture.md section 8, item 4a).
pub fn toolchain_tree_digest(dir: &Path) -> Result<String, String> {
    let uid = dfm_platform_macos::current_uid()
        .ok_or_else(|| "cannot determine the current user".to_string())?;
    toolchain_tree_digest_as(dir, uid)
}

fn toolchain_tree_digest_as(dir: &Path, uid: u32) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    closed_tree(dir, ".", uid)?;
    let mut entries = Vec::new();
    for (rel, node) in walk(dir)? {
        let kind = match node {
            Node::File => {
                let p = dir.join(&rel);
                let exec = fs::metadata(&p)
                    .map_err(|e| format!("{rel}: {}", e.kind()))?
                    .permissions()
                    .mode()
                    & 0o100
                    != 0;
                EntryKind::File {
                    exec,
                    sha256: sha256_file(&p).map_err(|e| format!("{rel}: {}", e.kind()))?,
                }
            }
            Node::Link(target) => {
                if !target.bytes().all(|b| (0x20..=0x7e).contains(&b))
                    || !controls::path_ok(&target, rel.matches('/').count())
                {
                    return Err(format!("symlink {rel} leaves the toolchain tree"));
                }
                EntryKind::Symlink { target }
            }
        };
        entries.push(TreeEntry { path: rel, kind });
    }
    // Parity with the shell checker: an empty or symlink-only tree is not a toolchain.
    if !entries
        .iter()
        .any(|e| matches!(e.kind, EntryKind::File { .. }))
    {
        return Err("the toolchain tree has no regular file".into());
    }
    Ok(tree_digest(entries))
}

/// The build directory as the kernel will report it, even if it does not exist yet.
fn canonical_lenient(p: &Path) -> PathBuf {
    let (mut cur, mut tail) = (p.to_path_buf(), Vec::new());
    loop {
        if let Ok(c) = fs::canonicalize(&cur) {
            return tail.iter().rev().fold(c, |acc, t| acc.join(t));
        }
        match cur.file_name().map(|n| n.to_owned()) {
            Some(n) if cur.pop() => tail.push(n),
            _ => return p.to_path_buf(),
        }
    }
}

fn ancestors(platform: &Platform) -> Result<(), Refusal> {
    let build = canonical_lenient(&platform.build_dir());
    for dir in build.ancestors() {
        for f in [
            ".cargo/config.toml",
            ".cargo/config",
            "rust-toolchain",
            "rust-toolchain.toml",
        ] {
            match fs::symlink_metadata(dir.join(f)) {
                Ok(_) => {
                    return Err(Refusal::new(
                        Code::BuildConfig,
                        format!(
                            "{} would be read by Cargo or rustup for the build directory {}",
                            dir.join(f).display(),
                            build.display()
                        ),
                    ));
                }
                Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {}
                // An ancestor that cannot be searched cannot be shown to be free of config.
                Err(e) => {
                    return Err(Refusal::new(
                        Code::BuildConfig,
                        format!("cannot check {}: {}", dir.join(f).display(), e.kind()),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// `self-check --build-controls`: staged tree, toolchain tree digest, ancestors, in that order.
pub fn check_build_controls(platform: &Platform, stage: &Path) -> Result<(), Refusal> {
    // A failed `id -u` is an environment failure, not evidence about the toolchain.
    let uid = dfm_platform_macos::current_uid()
        .ok_or_else(|| Refusal::new(Code::IoFailed, "cannot determine the current user"))?;
    check_build_controls_as(platform, stage, uid)
}

fn check_build_controls_as(platform: &Platform, stage: &Path, uid: u32) -> Result<(), Refusal> {
    let (pin, want) = staged_tree(stage)?;
    let mismatch = |d: String| Refusal::new(Code::ToolchainMismatch, d);
    let dir = platform.toolchain_dir(&pin);
    if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
        return Err(mismatch(format!(
            "toolchain {pin} is absent at {}; install exactly that version there",
            dir.display()
        )));
    }
    closed_ancestors(platform.home(), &dir, uid).map_err(mismatch)?;
    if toolchain_tree_digest_as(&dir, uid).map_err(mismatch)? != want {
        return Err(Refusal::new(
            Code::ToolchainMismatch,
            format!("toolchain {pin} tree digest differs from the lock"),
        ));
    }
    ancestors(platform)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_toolchain_entry_with_another_owner_is_refused_and_named() {
        let d = std::env::temp_dir().join(format!("dfm-closed-tree-{}", std::process::id()));
        fs::create_dir_all(d.join("bin")).unwrap();
        fs::write(d.join("bin/a"), "a").unwrap();
        std::os::unix::fs::symlink("a", d.join("bin/l")).unwrap();
        let me = dfm_platform_macos::current_uid().unwrap();
        // A test cannot create a file owned by someone else, so the expected owner is the other one.
        let other = me + 1;
        assert_eq!(closed_tree(&d, ".", me), Ok(()));
        assert_eq!(
            closed_tree(&d, ".", other),
            Err(". is not owned by the current user".to_string())
        );
        for (rel, want) in [("bin/a", "./bin/a"), ("bin/l", "./bin/l"), ("bin", "./bin")] {
            let md = fs::symlink_metadata(d.join(rel)).unwrap();
            assert_eq!(closed_entry(&md, want, me), Ok(()), "{rel}");
            assert_eq!(
                closed_entry(&md, want, other),
                Err(format!("{want} is not owned by the current user")),
                "{rel}"
            );
        }
        // Mode is still refused for a file, and ignored for a link.
        fs::set_permissions(d.join("bin/a"), fs::Permissions::from_mode(0o666)).unwrap();
        let md = fs::symlink_metadata(d.join("bin/a")).unwrap();
        assert!(closed_entry(&md, "./bin/a", me).is_err());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_digest_refuses_a_toolchain_that_another_user_owns() {
        let d = std::env::temp_dir().join(format!("dfm-digest-owner-{}", std::process::id()));
        fs::create_dir_all(d.join("bin")).unwrap();
        fs::write(d.join("bin/a"), "a").unwrap();
        let me = dfm_platform_macos::current_uid().unwrap();
        assert!(toolchain_tree_digest_as(&d, me).is_ok());
        // The digest function itself must call the owner check; a test cannot create foreign files.
        assert_eq!(
            toolchain_tree_digest_as(&d, me + 1),
            Err(". is not owned by the current user".to_string())
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_linked_foreign_or_open_ancestor_of_the_toolchain_is_refused() {
        let root = std::env::temp_dir().join(format!("dfm-ancestors-{}", std::process::id()));
        let home = root.join("home");
        let tc = home.join(".local/share/dotfiles-manager/toolchains/1.0.0");
        fs::create_dir_all(&tc).unwrap();
        let me = dfm_platform_macos::current_uid().unwrap();
        assert_eq!(closed_ancestors(&home, &tc, me), Ok(()));
        let err = closed_ancestors(&home, &tc, me + 1).unwrap_err();
        assert!(
            err.ends_with(".local is not owned by the current user"),
            "{err}"
        );
        let share = home.join(".local/share");
        fs::set_permissions(&share, fs::Permissions::from_mode(0o775)).unwrap();
        let err = closed_ancestors(&home, &tc, me).unwrap_err();
        assert!(
            err.ends_with("/share is writable by group or others"),
            "{err}"
        );
        fs::set_permissions(&share, fs::Permissions::from_mode(0o755)).unwrap();
        // A link in the chain is refused, even when its target is a fine directory.
        let real = root.join("real");
        fs::rename(home.join(".local"), &real).unwrap();
        std::os::unix::fs::symlink(&real, home.join(".local")).unwrap();
        let err = closed_ancestors(&home, &tc, me).unwrap_err();
        assert!(err.ends_with(".local is a symlink"), "{err}");
        // An ancestor that does not exist yet is not an error here.
        assert_eq!(closed_ancestors(&home, &home.join("x/y/z"), me), Ok(()));
        fs::remove_dir_all(&root).unwrap();
    }
}
