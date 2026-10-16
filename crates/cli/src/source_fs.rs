//! Reads a materialized source directory into an in-memory `SourceSet` for the resolver.
//! Read-only. A bundle's `[sources]` table limits the walk to its roots and skips excluded
//! paths before they are listed or opened (docs/resolver.md, "Source roots and excludes").
//! Symlinks are recorded, never followed, so the resolver decides containment. An excluded
//! symlink is recorded too, apart from the hashed entries, because exclusion never hides a link
//! from the containment check.
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

use crate::read_fs::read_regular_bounded;
use dfm_model::fold::fold;
use dfm_model::resolve::{SourceScope, SourceSet, bundle_scope, profile_bundle_path};
use dfm_model::{Code, Refusal};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use dfm_model::plan::{
    MAX_SOURCE_ENTRIES as MAX_ENTRIES, MAX_SOURCE_FILE_BYTES as MAX_FILE_BYTES,
    MAX_SOURCE_TOTAL_BYTES as MAX_TOTAL_BYTES,
};

/// Reads at most `MAX_FILE_BYTES + 1` bytes of a regular file. The open does not follow a final
/// symlink and does not block on a FIFO, and fstat then confirms the type, so an entry that
/// changed type after the directory listing is refused instead of read.
fn read_regular(path: &Path, shown: &str) -> Result<Vec<u8>, Refusal> {
    read_regular_bounded(
        path,
        MAX_FILE_BYTES,
        |e| {
            refusal(
                Code::ReportUnavailable,
                format!("cannot read source {shown}: {}", e.kind()),
            )
        },
        || {
            refusal(
                Code::SourceUntrusted,
                format!("{shown} changed type while reading"),
            )
        },
    )
}

/// A refusal whose detail may hold control, bidirectional or zero-width characters from the tree
/// being read. Every command escapes a detail once, when it prints it, so it is not escaped here.
fn refusal(code: Code, detail: String) -> Refusal {
    Refusal::new(code, detail)
}

/// Reads the parts of `root` that the profile's bundle declares in `[sources]`, or the whole
/// root when it declares none. `profile` is the root-relative profile path. A profile or bundle
/// that cannot be found here is not an error yet: the whole root is read and the resolver
/// reports the same fault it always did. A `[sources]` table that is invalid is refused here.
pub fn load_scoped(root: &Path, profile: &str) -> Result<SourceSet, Refusal> {
    match find_scope(root, profile)? {
        None => load(root, None),
        Some(scope) => load(root, Some(&scope)),
    }
}

/// The `[sources]` declaration of the bundle that `profile` names. The resolver later checks
/// that it reached the same bundle (`E_MANIFEST_INVALID` otherwise).
fn find_scope(root: &Path, profile: &str) -> Result<Option<SourceScope>, Refusal> {
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
    Ok(bundle_scope(bundle, &bytes)?)
}

/// Each declared root must be a real directory: no link in any component, nothing missing.
/// Roots nested inside another root are dropped so no entry is read twice.
fn walkable_roots(root: &Path, scope: &SourceScope) -> Result<Vec<String>, Refusal> {
    let bundle = scope.file();
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
            let meta = fs::symlink_metadata(&at).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    fault(
                        Code::ManifestInvalid,
                        r,
                        "does not exist",
                        "create the directory or remove it from `roots`",
                    )
                } else {
                    fault(
                        Code::ReportUnavailable,
                        r,
                        &format!("cannot be read: {}", e.kind()),
                        "fix the permissions of the directory",
                    )
                }
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
    pruned: usize,
    /// Where the walk starts: directory, its root-relative name, and whether it is shallow.
    starts: Vec<(PathBuf, String, bool)>,
}

/// Entries counted before the size report stops. The report lists names only and opens no
/// file, but a tree can be arbitrarily large.
const COUNT_CEILING: usize = 1_000_000;

/// Excluded entries recorded for the plan note, over all loaded directories.
const MAX_PRUNED: usize = 100_000;

/// What the size report learned: direct entries per directory, how many entries it saw
/// (stopping at the ceiling), and how many directories it could not list.
struct Counts {
    per_dir: BTreeMap<String, usize>,
    seen: usize,
    unreadable: usize,
}

/// Lists directories under the walk starts and counts direct entries. It opens no file and
/// skips what the loader skips. It stops at `ceiling` entries.
fn count_entries(
    starts: &[(PathBuf, String, bool)],
    scope: Option<&SourceScope>,
    ceiling: usize,
) -> Counts {
    fn go(
        dir: &Path,
        rel: &str,
        shallow: bool,
        scope: Option<&SourceScope>,
        ceiling: usize,
        c: &mut Counts,
    ) {
        let Ok(rd) = fs::read_dir(dir) else {
            c.unreadable += 1;
            return;
        };
        for e in rd {
            let Ok(e) = e else {
                c.unreadable += 1;
                continue;
            };
            if c.seen >= ceiling {
                return;
            }
            let name = e.file_name().to_string_lossy().into_owned();
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
            c.seen += 1;
            *c.per_dir.entry(rel.to_string()).or_default() += 1;
            if is_dir {
                go(&e.path(), &path, false, scope, ceiling, c);
            }
        }
    }
    let mut c = Counts {
        per_dir: BTreeMap::new(),
        seen: 0,
        unreadable: 0,
    };
    for (dir, rel, shallow) in starts {
        go(dir, rel, *shallow, scope, ceiling, &mut c);
    }
    c
}

impl State<'_> {
    /// The refusal for a tree over the cap. A second pass that lists directories but opens no
    /// file counts the entries per directory, so the message names the real largest ones.
    fn too_many_entries(&self) -> Refusal {
        let c = count_entries(&self.starts, self.scope, COUNT_CEILING);
        let mut dirs: Vec<_> = c.per_dir.iter().collect();
        dirs.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let top: Vec<String> = dirs
            .iter()
            .take(3)
            .map(|(d, n)| {
                let d = if d.is_empty() { "(root)" } else { d.as_str() };
                format!("{d} ({n} entries)")
            })
            .collect();
        let at_least = if c.seen >= COUNT_CEILING {
            "at least "
        } else {
            ""
        };
        let unreadable = if c.unreadable > 0 {
            format!(
                " ({} directories could not be listed and are not counted)",
                c.unreadable
            )
        } else {
            String::new()
        };
        refusal(
            Code::LimitExceeded,
            format!(
                "source tree has more than {MAX_ENTRIES} entries. Largest directories ({at_least}direct entries): {}{unreadable}. Fix: list the trees you need in `[sources].roots`, or skip these with `[sources].exclude` in the bundle",
                top.join(", ")
            ),
        )
    }
}

fn load(root: &Path, declared: Option<&SourceScope>) -> Result<SourceSet, Refusal> {
    fn io(e: std::io::Error, p: &str) -> Refusal {
        refusal(
            Code::ReportUnavailable,
            format!("cannot read source {p}: {}", e.kind()),
        )
    }
    /// The target text of the link `e`, which must be UTF-8 and hold no control character, like a
    /// name. An excluded link is read the same way, because containment is decided on the text.
    fn link_target(e: &fs::DirEntry, path: &str, rel: &str) -> Result<String, Refusal> {
        let target = fs::read_link(e.path()).map_err(|e| io(e, path))?;
        let target = target.into_os_string().into_string().map_err(|_| {
            refusal(
                Code::SourceUntrusted,
                format!("non-UTF-8 link target at {path}"),
            )
        })?;
        if target.chars().any(char::is_control) {
            return Err(refusal(
                Code::SourceUntrusted,
                format!("control character in a name under {rel}"),
            ));
        }
        Ok(target)
    }
    /// Reads the entries of `dir`. With `shallow`, subdirectories are left alone.
    fn go(dir: &Path, rel: &str, st: &mut State, shallow: bool) -> Result<(), Refusal> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .map_err(|e| io(e, rel))?
            .collect::<Result<_, _>>()
            .map_err(|e| io(e, rel))?;
        names.sort_by_key(|e| e.file_name());
        // Two names that differ only by case are one name to a case-insensitive filesystem, and
        // the snapshot indexes directories by folded path. Refuse them instead of guessing.
        let mut folded: BTreeMap<String, String> = BTreeMap::new();
        for e in names {
            // An excluded entry is skipped before its name is validated and before it is listed,
            // opened or counted, so a name that would be refused elsewhere does not abort the load.
            if let Some(sc) = st.scope {
                let lossy = e.file_name().to_string_lossy().into_owned();
                let shown = if rel.is_empty() {
                    lossy
                } else {
                    format!("{rel}/{lossy}")
                };
                if sc.excluded_by(&shown).is_some() {
                    st.pruned += 1;
                    if st.pruned > MAX_PRUNED {
                        return Err(refusal(
                            Code::LimitExceeded,
                            format!(
                                "more than {MAX_PRUNED} excluded entries. Fix: exclude their parent directory instead"
                            ),
                        ));
                    }
                    st.set.record_pruned(&shown);
                    // Exclusion hides an entry from hashing, never from containment: a link
                    // that is excluded is still published inside a directory source. An excluded
                    // directory is not listed, so the resolver needs to know it is there.
                    let ty = e.file_type().map_err(|e| io(e, &shown))?;
                    if ty.is_symlink() {
                        let target = link_target(&e, &shown, rel)?;
                        st.set.record_pruned_link(&shown, &target);
                    } else if ty.is_dir() {
                        st.set.record_pruned_dir(&shown);
                    }
                    continue;
                }
            }
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
            if let Some(other) = folded.insert(fold(&name), name.clone()) {
                return Err(refusal(
                    Code::SourceUntrusted,
                    format!("{other} and {name} in {rel} differ only by case"),
                ));
            }
            if name.to_lowercase() == ".git" {
                // Repository metadata beside the bundle or at the top is not source content.
                if rel.is_empty() || shallow {
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
                let target = link_target(&e, &path, rel)?;
                st.set.insert_link(path, target);
            } else if ty.is_dir() {
                st.set.insert_dir(path.clone());
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
        scope: declared,
        entries: 0,
        bytes: 0,
        pruned: 0,
        starts: Vec::new(),
    };
    match declared {
        None => {
            st.starts.push((root.to_path_buf(), String::new(), false));
            go(root, "", &mut st, false)?;
        }
        Some(scope) => {
            let roots = walkable_roots(root, scope)?;
            for r in &roots {
                st.starts.push((root.join(r), r.clone(), false));
                // A root with nothing loaded below it is still a directory.
                st.set.insert_dir(r.clone());
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
    st.set.set_scope(declared.cloned());
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
    fn names_that_differ_only_by_case_are_refused() {
        let d = scratch();
        fs::create_dir(d.join("Dir")).unwrap();
        // A case-insensitive volume cannot hold both names.
        if fs::create_dir(d.join("dir")).is_err() || fs::read_dir(d.join("Dir")).is_err() {
            eprintln!("SKIPPED: case-insensitive volume");
            let _ = fs::remove_dir_all(&d);
            return;
        }
        fs::write(d.join("Dir/a"), "a").unwrap();
        let e = load(&d, None).unwrap_err();
        assert_eq!(e.code, Code::SourceUntrusted);
        assert!(e.detail.contains("differ only by case"), "{}", e.detail);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn git_metadata_is_matched_without_regard_to_case() {
        let d = scratch();
        fs::create_dir_all(d.join(".Git")).unwrap();
        fs::write(d.join(".Git/config"), "x").unwrap();
        fs::write(d.join("a"), "a").unwrap();
        let set = load(&d, None).unwrap();
        assert_eq!(set.paths().collect::<Vec<_>>(), ["a"]);
        fs::create_dir_all(d.join("m/.GIT")).unwrap();
        assert_eq!(load(&d, None).unwrap_err().code, Code::SourceUntrusted);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn unreadable_roots_are_report_unavailable_with_exit_2() {
        let e = load(Path::new("/nonexistent-dfm-root"), None).unwrap_err();
        assert_eq!(e.code, Code::ReportUnavailable);
        assert_eq!(e.code.exit().code(), 2);
    }

    #[test]
    fn the_size_report_stops_at_its_ceiling_and_counts_unreadable_directories() {
        let d = scratch();
        for i in 0..10 {
            fs::write(d.join(format!("f{i}")), "").unwrap();
        }
        fs::create_dir(d.join("sub")).unwrap();
        fs::write(d.join("sub/x"), "").unwrap();
        let start = [(d.clone(), String::new(), false)];
        let all = count_entries(&start, None, 1_000);
        assert_eq!((all.seen, all.per_dir[""], all.per_dir["sub"]), (12, 11, 1));
        let few = count_entries(&start, None, 3);
        assert_eq!(few.seen, 3);
        // A directory that cannot be listed is counted, not skipped silently.
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(d.join("sub"), fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = count_entries(&start, None, 1_000);
        fs::set_permissions(d.join("sub"), fs::Permissions::from_mode(0o755)).unwrap();
        let root = Command::new("id").arg("-u").output().unwrap().stdout == b"0\n";
        assert_eq!(unreadable.unreadable, usize::from(!root));
        let _ = fs::remove_dir_all(&d);
    }
}
