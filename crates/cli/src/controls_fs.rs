//! Reads a staged tree and the toolchain tree from disk and applies the build
//! controls in `dfm_model::controls` (docs/architecture.md section 8, item 3a).
//! Read-only. Starts no process.

use dfm_model::controls::{self, MAX_FILE_BYTES};
use dfm_model::digest::{EntryKind, TreeEntry, hex, tree_digest};
use dfm_model::{Code, Refusal};
use dfm_platform_macos::Platform;
use sha2::{Digest, Sha256};
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

fn file_sha256(path: &Path) -> std::io::Result<String> {
    let mut f = fs::File::open(path)?;
    let (mut h, mut buf) = (Sha256::new(), vec![0u8; 1 << 16]);
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(hex(h.finalize().as_slice()));
        }
        h.update(&buf[..n]);
    }
}

/// Digest of an installed toolchain tree (docs/architecture.md section 8, item 4a).
pub fn toolchain_tree_digest(dir: &Path) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    // The digest records only the owner-execute bit, so a tree that others can modify is
    // refused instead of being described.
    fn others_can_write(dir: &Path, rel: &str) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;
        let ioerr = |e: std::io::Error| format!("{rel}: {}", e.kind());
        if fs::symlink_metadata(dir)
            .map_err(ioerr)?
            .permissions()
            .mode()
            & 0o022
            != 0
        {
            return Err(format!("{rel} is writable by group or others"));
        }
        for e in fs::read_dir(dir).map_err(ioerr)? {
            let e = e.map_err(ioerr)?;
            let ty = e.file_type().map_err(ioerr)?;
            let name = format!("{rel}/{}", e.file_name().to_string_lossy());
            if ty.is_dir() {
                others_can_write(&e.path(), &name)?;
            } else if !ty.is_symlink()
                && e.metadata().map_err(ioerr)?.permissions().mode() & 0o022 != 0
            {
                return Err(format!("{name} is writable by group or others"));
            }
        }
        Ok(())
    }
    others_can_write(dir, ".")?;
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
                    sha256: file_sha256(&p).map_err(|e| format!("{rel}: {}", e.kind()))?,
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
    let (pin, want) = staged_tree(stage)?;
    let mismatch = |d: String| Refusal::new(Code::ToolchainMismatch, d);
    let dir = platform.toolchain_dir(&pin);
    if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
        return Err(mismatch(format!(
            "toolchain {pin} is absent at {}; install exactly that version there",
            dir.display()
        )));
    }
    if toolchain_tree_digest(&dir).map_err(mismatch)? != want {
        return Err(Refusal::new(
            Code::ToolchainMismatch,
            format!("toolchain {pin} tree digest differs from the lock"),
        ));
    }
    ancestors(platform)
}
