//! Reads a materialized source directory into an in-memory `SourceSet` for the resolver.
//! Read-only. A bundle's `[sources]` table limits the walk to its roots and skips excluded
//! paths before they are listed or opened (docs/resolver.md, "Source roots and excludes").
//! Symlinks are recorded, never followed, so the resolver decides containment.
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

use dfm_model::fold::fold;
use dfm_model::resolve::{SourceScope, SourceSet, bundle_scope, profile_bundle_path};
use dfm_model::{Code, Refusal};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const MAX_FILE_BYTES: u64 = 1 << 20;
const MAX_ENTRIES: usize = 20_000;
const MAX_TOTAL_BYTES: u64 = 64 << 20;

/// Reads at most `MAX_FILE_BYTES + 1` bytes of a regular file. The open does not follow a final
/// symlink and does not block on a FIFO, and fstat then confirms the type, so an entry that
/// changed type after the directory listing is refused instead of read.
fn read_regular(path: &Path, shown: &str) -> Result<Vec<u8>, Refusal> {
    let io = |e: std::io::Error| {
        refusal(
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
        return Err(refusal(
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

/// A refusal whose detail is safe to print: paths come from the tree being read and may hold
/// control, bidirectional or zero-width characters.
fn refusal(code: Code, detail: String) -> Refusal {
    Refusal::new(code, dfm_model::display::visible(&detail))
}

/// Reads the whole `root`, as a bundle without `[sources]` expects.
#[cfg(test)]
fn load_source_set(root: &Path) -> Result<SourceSet, Refusal> {
    load(root, None)
}

/// Reads the parts of `root` that the profile's bundle declares in `[sources]`, or the whole
/// root when it declares none. `profile` is the root-relative profile path. A profile or bundle
/// that cannot be found here is not an error yet: the whole root is read and the resolver
/// reports the same fault it always did. A `[sources]` table that is invalid is refused here.
pub fn load_scoped(root: &Path, profile: &str) -> Result<SourceSet, Refusal> {
    match find_scope(root, profile)? {
        None => load(root, None),
        Some((scope, bundle)) => load(root, Some((&scope, &bundle))),
    }
}

/// The `[sources]` declaration of the bundle that `profile` names, with that bundle's path.
fn find_scope(root: &Path, profile: &str) -> Result<Option<(SourceScope, String)>, Refusal> {
    let Ok(profile_bytes) = read_regular(&root.join(profile), profile) else {
        return Ok(None);
    };
    let Some(bundle) = profile_bundle_path(profile, &profile_bytes) else {
        return Ok(None);
    };
    // The operating system follows links in the path; the result must still be under `root`.
    let Ok(real) = fs::canonicalize(root.join(&bundle)) else {
        return Ok(None);
    };
    let Some(bundle) = real.strip_prefix(root).ok().and_then(Path::to_str) else {
        return Ok(None);
    };
    let Ok(bytes) = read_regular(&real, bundle) else {
        return Ok(None);
    };
    Ok(bundle_scope(bundle, &bytes)?.map(|s| (s, bundle.to_string())))
}

/// Each declared root must be a real directory: no link in any component, nothing missing.
/// Roots nested inside another root are dropped so no entry is read twice.
fn walkable_roots(root: &Path, scope: &SourceScope, bundle: &str) -> Result<Vec<String>, Refusal> {
    let fault = |code, r: &str, why: &str, fix: &str| {
        refusal(
            code,
            format!(
                "[sources] root `{r}` {why} (source: {bundle}; field: /sources/roots). Fix: {fix}"
            ),
        )
    };
    let mut kept: Vec<String> = Vec::new();
    for r in scope.roots() {
        let mut at = root.to_path_buf();
        for c in r.split('/') {
            at.push(c);
            let meta = fs::symlink_metadata(&at).map_err(|_| {
                fault(
                    Code::ManifestInvalid,
                    r,
                    "does not exist",
                    "create the directory or remove it from `roots`",
                )
            })?;
            if meta.file_type().is_symlink() {
                return Err(fault(
                    Code::SourceEscape,
                    r,
                    "is or passes through a link",
                    "name the real directory",
                ));
            }
            if !meta.is_dir() {
                return Err(fault(
                    Code::ManifestInvalid,
                    r,
                    "is not a directory",
                    "name a directory",
                ));
            }
        }
        kept.push(r.clone());
    }
    let nested = |a: &str, b: &str| {
        let (a, b) = (fold(a), fold(b));
        a != b && a.starts_with(&format!("{b}/"))
    };
    let mut out: Vec<String> = Vec::new();
    for r in &kept {
        if !kept.iter().any(|o| nested(r, o)) && !out.iter().any(|o| fold(o) == fold(r)) {
            out.push(r.clone());
        }
    }
    Ok(out)
}

struct State<'a> {
    set: SourceSet,
    scope: Option<&'a SourceScope>,
    entries: usize,
    bytes: u64,
    /// Where the walk starts: directory, its root-relative name, and whether it is shallow.
    starts: Vec<(PathBuf, String, bool)>,
}

/// Directories counted before the size report stops. The report lists names only and opens no
/// file, but a tree can be arbitrarily large.
const COUNT_CEILING: usize = 1_000_000;

impl State<'_> {
    /// The refusal for a tree over the cap. A second pass that lists directories but opens no
    /// file counts the entries per directory, so the message names the real largest ones.
    fn too_many_entries(&self) -> Refusal {
        fn count(
            dir: &Path,
            rel: &str,
            shallow: bool,
            scope: Option<&SourceScope>,
            seen: &mut usize,
            per_dir: &mut BTreeMap<String, usize>,
        ) {
            let Ok(rd) = fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                if *seen >= COUNT_CEILING {
                    return;
                }
                let Ok(name) = e.file_name().into_string() else {
                    continue;
                };
                let path = if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{rel}/{name}")
                };
                if name.to_lowercase() == ".git"
                    || scope.is_some_and(|s| s.excluded_by(&path).is_some())
                {
                    continue;
                }
                let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
                if shallow && is_dir {
                    continue;
                }
                *seen += 1;
                *per_dir.entry(rel.to_string()).or_default() += 1;
                if is_dir {
                    count(&e.path(), &path, false, scope, seen, per_dir);
                }
            }
        }
        let (mut seen, mut per_dir) = (0, BTreeMap::new());
        for (dir, rel, shallow) in &self.starts {
            count(dir, rel, *shallow, self.scope, &mut seen, &mut per_dir);
        }
        let mut dirs: Vec<_> = per_dir.iter().collect();
        dirs.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let top: Vec<String> = dirs
            .iter()
            .take(3)
            .map(|(d, n)| {
                let d = if d.is_empty() { "(root)" } else { d.as_str() };
                format!("{d} ({n} entries)")
            })
            .collect();
        let at_least = if seen >= COUNT_CEILING {
            "at least "
        } else {
            ""
        };
        refusal(
            Code::LimitExceeded,
            format!(
                "source tree has more than {MAX_ENTRIES} entries. Largest directories ({at_least}direct entries): {}. Fix: list the trees you need in `[sources].roots`, or skip these with `[sources].exclude` in the bundle",
                top.join(", ")
            ),
        )
    }
}

fn load(root: &Path, declared: Option<(&SourceScope, &str)>) -> Result<SourceSet, Refusal> {
    fn io(e: std::io::Error, p: &str) -> Refusal {
        refusal(
            Code::ReportUnavailable,
            format!("cannot read source {p}: {}", e.kind()),
        )
    }
    /// Reads the entries of `dir`. With `shallow`, subdirectories are left alone.
    fn go(dir: &Path, rel: &str, st: &mut State, shallow: bool) -> Result<(), Refusal> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .map_err(|e| io(e, rel))?
            .collect::<Result<_, _>>()
            .map_err(|e| io(e, rel))?;
        names.sort_by_key(|e| e.file_name());
        for e in names {
            let name = e
                .file_name()
                .into_string()
                .map_err(|_| refusal(Code::SourceUntrusted, format!("non-UTF-8 name in {rel}")))?;
            if name.chars().any(char::is_control) {
                return Err(refusal(
                    Code::SourceUntrusted,
                    format!("control character in a name under {rel}"),
                ));
            }
            let path = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            // An excluded entry is skipped before anything is opened, listed or counted.
            if st.scope.is_some_and(|s| s.excluded_by(&path).is_some()) {
                continue;
            }
            if name.to_lowercase() == ".git" {
                if rel.is_empty() {
                    continue;
                }
                return Err(refusal(
                    Code::SourceUntrusted,
                    format!("nested .git under {rel}: submodules and gitlinks are refused"),
                ));
            }
            let ty = e.file_type().map_err(|e| io(e, &path))?;
            if shallow && ty.is_dir() {
                continue;
            }
            st.entries += 1;
            if st.entries > MAX_ENTRIES {
                return Err(st.too_many_entries());
            }
            if ty.is_symlink() {
                let target = fs::read_link(e.path()).map_err(|e| io(e, &path))?;
                let target = target.into_os_string().into_string().map_err(|_| {
                    refusal(
                        Code::SourceUntrusted,
                        format!("non-UTF-8 link target at {path}"),
                    )
                })?;
                st.set.insert_link(path, target);
            } else if ty.is_dir() {
                go(&e.path(), &path, st, false)?;
            } else if ty.is_file() {
                let buf = read_regular(&e.path(), &path)?;
                st.bytes += buf.len() as u64;
                if buf.len() as u64 > MAX_FILE_BYTES || st.bytes > MAX_TOTAL_BYTES {
                    return Err(refusal(
                        Code::LimitExceeded,
                        format!("source {path} exceeds the read limit"),
                    ));
                }
                st.set.insert_file(path, buf);
            } else {
                return Err(refusal(
                    Code::SourceUntrusted,
                    format!("special file at {path}"),
                ));
            }
        }
        Ok(())
    }
    let mut st = State {
        set: SourceSet::new(),
        scope: declared.map(|(s, _)| s),
        entries: 0,
        bytes: 0,
        starts: Vec::new(),
    };
    match declared {
        None => {
            st.starts.push((root.to_path_buf(), String::new(), false));
            go(root, "", &mut st, false)?;
        }
        Some((scope, bundle)) => {
            let roots = walkable_roots(root, scope, bundle)?;
            for r in &roots {
                st.starts.push((root.join(r), r.clone(), false));
            }
            // The bundle directory contributes its own files and links, unless a root holds it.
            let dir = scope.bundle_dir();
            let held = roots
                .iter()
                .any(|r| fold(dir) == fold(r) || fold(dir).starts_with(&format!("{}/", fold(r))));
            if !held {
                st.starts.push((root.join(dir), dir.to_string(), true));
            }
            for (path, rel, shallow) in st.starts.clone() {
                go(&path, &rel, &mut st, shallow)?;
            }
        }
    }
    st.set.set_scope(declared.map(|(s, _)| s.clone()));
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
