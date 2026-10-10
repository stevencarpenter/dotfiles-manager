//! Reads a materialized source directory into an in-memory `SourceSet` for the resolver.
//! Read-only. Symlinks are recorded, never followed, so the resolver decides containment.
//! A top-level `.git` (any letter case) is repository metadata, not source content, and is
//! skipped. A `.git` anywhere deeper is a submodule or gitlink, which the tree rules refuse
//! (E_SOURCE_UNTRUSTED, docs/architecture.md section 8).
//!
//! Each file is opened with `O_NOFOLLOW` and `O_NONBLOCK` and checked with fstat. `O_NOFOLLOW`
//! covers only the last path component: an entry swapped for a link or FIFO after listing is
//! refused, but a parent directory swapped for a link would still be followed. The loader
//! assumes nobody rewrites the tree while it runs; containment decisions are made later, on the
//! snapshot. A source that cannot be read is `E_REPORT_UNAVAILABLE`, exit 2, which is
//! docs/architecture.md section 10: "inability to produce the requested report".

use dfm_model::resolve::SourceSet;
use dfm_model::{Code, Refusal};
use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const MAX_FILE_BYTES: u64 = 1 << 20;
const MAX_ENTRIES: usize = 20_000;
const MAX_TOTAL_BYTES: u64 = 64 << 20;

/// Reads at most `MAX_FILE_BYTES + 1` bytes of a regular file. The open does not follow a final
/// symlink and does not block on a FIFO, and fstat then confirms the type, so an entry that
/// changed type after the directory listing is refused instead of read.
fn read_regular(path: &Path, shown: &str) -> Result<Vec<u8>, Refusal> {
    let io = |e: std::io::Error| {
        Refusal::new(
            Code::ReportUnavailable,
            format!("cannot read source {shown}: {}", e.kind()),
        )
    };
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(dfm_platform_macos::O_NOFOLLOW | dfm_platform_macos::O_NONBLOCK)
        .open(path)
        .map_err(io)?;
    if !file.metadata().map_err(io)?.is_file() {
        return Err(Refusal::new(
            Code::SourceUntrusted,
            format!("{shown} changed type while reading"),
        ));
    }
    let mut buf = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(io)?;
    Ok(buf)
}

pub fn load_source_set(root: &Path) -> Result<SourceSet, Refusal> {
    struct State {
        set: SourceSet,
        entries: usize,
        bytes: u64,
    }
    fn refuse(code: Code, what: String) -> Refusal {
        Refusal::new(code, what)
    }
    fn go(dir: &Path, rel: &str, st: &mut State) -> Result<(), Refusal> {
        let io = |e: std::io::Error, p: &str| {
            refuse(
                Code::ReportUnavailable,
                format!("cannot read source {p}: {}", e.kind()),
            )
        };
        let mut names: Vec<_> = fs::read_dir(dir)
            .map_err(|e| io(e, rel))?
            .collect::<Result<_, _>>()
            .map_err(|e| io(e, rel))?;
        names.sort_by_key(|e| e.file_name());
        for e in names {
            let name = e
                .file_name()
                .into_string()
                .map_err(|_| refuse(Code::SourceUntrusted, format!("non-UTF-8 name in {rel}")))?;
            if name.chars().any(char::is_control) {
                return Err(refuse(
                    Code::SourceUntrusted,
                    format!("control character in a name under {rel}"),
                ));
            }
            if name.to_lowercase() == ".git" {
                if rel.is_empty() {
                    continue;
                }
                return Err(refuse(
                    Code::SourceUntrusted,
                    format!("nested .git under {rel}: submodules and gitlinks are refused"),
                ));
            }
            let path = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            st.entries += 1;
            if st.entries > MAX_ENTRIES {
                return Err(refuse(
                    Code::LimitExceeded,
                    "source tree has too many entries".into(),
                ));
            }
            let ty = e.file_type().map_err(|e| io(e, &path))?;
            if ty.is_symlink() {
                let target = fs::read_link(e.path()).map_err(|e| io(e, &path))?;
                let target = target.into_os_string().into_string().map_err(|_| {
                    refuse(
                        Code::SourceUntrusted,
                        format!("non-UTF-8 link target at {path}"),
                    )
                })?;
                st.set.insert_link(path, target);
            } else if ty.is_dir() {
                go(&e.path(), &path, st)?;
            } else if ty.is_file() {
                let buf = read_regular(&e.path(), &path)?;
                st.bytes += buf.len() as u64;
                if buf.len() as u64 > MAX_FILE_BYTES || st.bytes > MAX_TOTAL_BYTES {
                    return Err(refuse(
                        Code::LimitExceeded,
                        format!("source {path} exceeds the read limit"),
                    ));
                }
                st.set.insert_file(path, buf);
            } else {
                return Err(refuse(
                    Code::SourceUntrusted,
                    format!("special file at {path}"),
                ));
            }
        }
        Ok(())
    }
    let mut st = State {
        set: SourceSet::new(),
        entries: 0,
        bytes: 0,
    };
    go(root, "", &mut st)?;
    Ok(st.set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn scratch() -> std::path::PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "dfm-srcfs-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn an_entry_that_changed_type_after_listing_is_refused_not_read() {
        let d = scratch();
        // A directory opens read-only, so only the fstat check can stop it.
        fs::create_dir(d.join("was-a-file")).unwrap();
        let e = read_regular(&d.join("was-a-file"), "was-a-file").unwrap_err();
        assert_eq!(
            (e.code, e.detail.contains("changed type")),
            (Code::SourceUntrusted, true)
        );
        // A FIFO opens without blocking, and fstat refuses it.
        let fifo = d.join("pipe");
        assert!(
            Command::new("/usr/bin/mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let e = read_regular(&fifo, "pipe").unwrap_err();
        assert_eq!(e.code, Code::SourceUntrusted);
        // A symlink is not followed: the open fails with ELOOP.
        fs::write(d.join("real"), "x").unwrap();
        symlink(d.join("real"), d.join("link")).unwrap();
        let e = read_regular(&d.join("link"), "link").unwrap_err();
        assert_eq!(e.code, Code::ReportUnavailable);
        // And a regular file reads.
        assert_eq!(read_regular(&d.join("real"), "real").unwrap(), b"x");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn git_metadata_is_matched_without_regard_to_case() {
        let d = scratch();
        fs::create_dir_all(d.join(".Git")).unwrap();
        fs::write(d.join(".Git/config"), "x").unwrap();
        fs::write(d.join("a"), "a").unwrap();
        let set = load_source_set(&d).unwrap();
        assert_eq!(set.paths().collect::<Vec<_>>(), ["a"]);
        fs::create_dir_all(d.join("m/.GIT")).unwrap();
        assert_eq!(load_source_set(&d).unwrap_err().code, Code::SourceUntrusted);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn unreadable_roots_are_report_unavailable_with_exit_2() {
        let e = load_source_set(Path::new("/nonexistent-dfm-root")).unwrap_err();
        assert_eq!(e.code, Code::ReportUnavailable);
        assert_eq!(e.code.exit().code(), 2);
    }
}
