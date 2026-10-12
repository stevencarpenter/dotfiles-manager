//! An in-memory snapshot of trusted, materialized sources. The resolver reads only this value:
//! it never touches the filesystem. Paths are `/`-separated and relative to the snapshot root.
//! Symlinks are kept as entries so containment can be decided without following the real ones.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    File(Vec<u8>),
    Link(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceSet {
    entries: BTreeMap<String, Entry>,
    /// Lowercased path to the spellings that fold to it. A case-insensitive filesystem resolves
    /// `Files/Link` and `files/link` to one entry, so lookups go through this index.
    folded: BTreeMap<String, Vec<String>>,
    /// Real directories the loader walked, by folded path. A directory with no loaded entry (an
    /// empty one, or one whose children are all excluded) exists only here.
    dirs: BTreeMap<String, String>,
    /// The bundle's `[sources]` declaration when the loader read only part of the root.
    scope: Option<SourceScope>,
    /// Folded paths of excluded entries that exist on disk and were not read. A directory source
    /// that holds some is not fully hashed.
    pruned: BTreeSet<String>,
    /// Excluded entries that are symlinks, by folded path and spelling, with the target text.
    /// Exclusion hides an entry from hashing only, so containment still applies to these links.
    /// Two spellings of one folded path are both kept: each one is checked.
    pruned_links: BTreeMap<(String, String), String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PathFault {
    /// Absolute, or climbs above the allowed root.
    Escape,
    /// Empty component, control or invisible character, or NUL.
    Invalid,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LocateFault {
    Escape,
    Missing,
    Loop,
    /// Two entries differ only by case, so a case-insensitive filesystem cannot tell them apart.
    CaseAlias,
    /// The loader did not read this path because the bundle's `[sources]` leaves it out.
    Scope(ScopeFault),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Located<'a> {
    File { path: String, bytes: &'a [u8] },
    Dir { path: String },
}

const MAX_HOPS: usize = 40;

use super::scope::{ScopeFault, SourceScope};
use crate::fold::fold;

impl SourceSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that only the parts of the root named by `scope` were loaded. Path lookups then
    /// refuse a path outside them as an escape instead of reporting it missing.
    pub fn set_scope(&mut self, scope: Option<SourceScope>) {
        self.scope = scope;
    }

    pub(crate) fn scope(&self) -> Option<&SourceScope> {
        self.scope.as_ref()
    }

    /// Records an excluded entry that exists but was left unread.
    pub fn record_pruned(&mut self, path: &str) {
        self.pruned.insert(fold(path));
    }

    /// Records an excluded entry that is a symlink, with its target text.
    pub fn record_pruned_link(&mut self, path: &str, target: &str) {
        self.pruned_links
            .insert((fold(path), path.to_string()), target.to_string());
    }

    /// Excluded symlinks below `dir` as (path, target text) pairs.
    pub(crate) fn pruned_links_below<'a>(
        &'a self,
        dir: &str,
    ) -> impl Iterator<Item = (&'a str, &'a str)> {
        let prefix = format!("{}/", fold(dir));
        self.pruned_links
            .range((prefix.clone(), String::new())..)
            .take_while(move |((folded, _), _)| folded.starts_with(&prefix))
            .map(|((_, p), t)| (p.as_str(), t.as_str()))
    }

    /// Excluded entries at or below `dir` that were left unread.
    pub fn pruned_under(&self, dir: &str) -> usize {
        let dir = fold(dir);
        let prefix = format!("{dir}/");
        usize::from(self.pruned.contains(&dir))
            + self
                .pruned
                .range(prefix.clone()..)
                .take_while(|k| k.starts_with(&prefix))
                .count()
    }

    fn insert(&mut self, path: String, entry: Entry) {
        let spellings = self.folded.entry(fold(&path)).or_default();
        if !spellings.contains(&path) {
            spellings.push(path.clone());
        }
        self.entries.insert(path, entry);
    }

    pub fn insert_file(&mut self, path: impl Into<String>, bytes: impl Into<Vec<u8>>) {
        self.insert(path.into(), Entry::File(bytes.into()));
    }

    pub fn insert_link(&mut self, path: impl Into<String>, target: impl Into<String>) {
        self.insert(path.into(), Entry::Link(target.into()));
    }

    /// Records a real directory, so an empty one is still a directory and part of the identity.
    pub fn insert_dir(&mut self, path: impl Into<String>) {
        let path = path.into();
        self.dirs.insert(fold(&path), path);
    }

    /// Every recorded directory, in folded-path order.
    pub fn dirs(&self) -> impl Iterator<Item = &str> {
        self.dirs.values().map(String::as_str)
    }

    /// Recorded directories below `dir`, as paths relative to `dir`.
    pub(crate) fn dirs_below<'a>(&'a self, dir: &str) -> impl Iterator<Item = &'a str> {
        let prefix = format!("{}/", fold(dir));
        let depth = dir.split('/').filter(|c| !c.is_empty()).count();
        self.dirs
            .range(prefix.clone()..)
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .filter_map(move |(_, v)| v.splitn(depth + 1, '/').nth(depth))
    }

    pub fn get(&self, path: &str) -> Option<&Entry> {
        self.entries.get(path)
    }

    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry at `path`, matched without regard to case.
    fn entry_at(&self, path: &str) -> Result<Option<(&String, &Entry)>, LocateFault> {
        match self.folded.get(&fold(path)).map(Vec::as_slice) {
            None | Some([]) => Ok(None),
            Some([one]) => Ok(self.entries.get_key_value(one)),
            Some(_) => Err(LocateFault::CaseAlias),
        }
    }

    fn is_dir(&self, path: &str) -> bool {
        let folded = fold(path);
        let prefix = format!("{folded}/");
        self.dirs.contains_key(&folded)
            || self
                .folded
                .range(prefix.clone()..)
                .next()
                .is_some_and(|(k, _)| k.starts_with(&prefix))
            || self
                .dirs
                .range(prefix.clone()..)
                .next()
                .is_some_and(|(k, _)| k.starts_with(&prefix))
    }

    /// Entries below `dir`, matched without regard to case, as paths relative to `dir`.
    pub(crate) fn below<'a>(&'a self, dir: &str) -> impl Iterator<Item = (&'a str, &'a Entry)> {
        let prefix = format!("{}/", fold(dir));
        let depth = dir.split('/').filter(|c| !c.is_empty()).count();
        self.folded
            .range(prefix.clone()..)
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .flat_map(|(_, spellings)| spellings)
            .filter_map(move |key| {
                let rel = key.splitn(depth + 1, '/').nth(depth)?;
                Some((rel, self.entries.get(key)?))
            })
    }

    /// Resolves symlinks inside the snapshot the way a filesystem would. Every hop must stay
    /// under `root`. A link that leaves it, an absolute link target, a loop, or two spellings
    /// that differ only by case is a fault. `..` is applied to the real directory reached so
    /// far, and only if that directory exists. `path` has been checked by [`join`].
    pub(crate) fn locate(&self, path: &str, root: &str) -> Result<Located<'_>, LocateFault> {
        let root_parts: Vec<&str> = root.split('/').filter(|c| !c.is_empty()).collect();
        let mut parts = path.split('/').filter(|c| !c.is_empty());
        if !root_parts.iter().all(|r| parts.next() == Some(*r)) {
            return Err(LocateFault::Escape);
        }
        let mut pending: std::collections::VecDeque<String> = parts.map(String::from).collect();
        let mut resolved: Vec<String> = root_parts.iter().map(|c| c.to_string()).collect();
        let mut hops = 0;
        while let Some(c) = pending.pop_front() {
            match c.as_str() {
                "." => continue,
                ".." => {
                    if resolved.len() <= root_parts.len() {
                        return Err(LocateFault::Escape);
                    }
                    if !self.is_dir(&resolved.join("/")) {
                        return Err(LocateFault::Missing);
                    }
                    resolved.pop();
                    continue;
                }
                _ => resolved.push(c),
            }
            if let Some(f) = self
                .scope
                .as_ref()
                .and_then(|s| s.fault(&resolved.join("/")))
            {
                return Err(LocateFault::Scope(f));
            }
            match self.entry_at(&resolved.join("/"))? {
                Some((_, Entry::Link(target))) => {
                    hops += 1;
                    if hops > MAX_HOPS {
                        return Err(LocateFault::Loop);
                    }
                    if target.starts_with('/') {
                        return Err(LocateFault::Escape);
                    }
                    resolved.pop();
                    for part in target.split('/').rev().filter(|p| !p.is_empty()) {
                        pending.push_front(part.to_string());
                    }
                }
                Some((key, Entry::File(_))) => {
                    // Keep the stored spelling so later steps and the result agree with it.
                    if let (Some(last), Some(actual)) =
                        (resolved.last_mut(), key.rsplit('/').next())
                    {
                        *last = actual.to_string();
                    }
                }
                None => {}
            }
        }
        let cur = resolved.join("/");
        match self.entry_at(&cur)? {
            Some((key, Entry::File(bytes))) => Ok(Located::File {
                path: key.clone(),
                bytes,
            }),
            Some((_, Entry::Link(_))) => Err(LocateFault::Missing),
            None if self.is_dir(&cur) => match &self.scope {
                // Only a root and its subtree were loaded whole. A directory above a root, the
                // bundle directory or a subdirectory beside the bundle holds loaded files in
                // part, so a source that names it would publish unchecked content.
                Some(sc) if !sc.holds_dir(&cur) => Err(LocateFault::Scope(sc.outside(&cur))),
                _ => Ok(Located::Dir { path: cur }),
            },
            None => Err(LocateFault::Missing),
        }
    }
}

/// Joins `rel` onto `base_dir` without interpreting `.` or `..`. A real filesystem resolves
/// `..` after it follows symlinks, so only [`SourceSet::locate`] may decide where such a path
/// ends up. Collapsing `..` here would let `files/link/../x` pass as `files/x`.
/// `root` is the directory the final path must stay under; `locate` enforces it.
pub(crate) fn join(base_dir: &str, rel: &str, _root: &str) -> Result<String, PathFault> {
    if rel.starts_with('/') {
        return Err(PathFault::Escape);
    }
    if rel.is_empty()
        || rel
            .chars()
            .any(|c| c.is_control() || super::error::is_invisible(c))
        || rel.split('/').any(str::is_empty)
    {
        return Err(PathFault::Invalid);
    }
    Ok(if base_dir.is_empty() {
        rel.to_string()
    } else {
        format!("{base_dir}/{rel}")
    })
}

pub(crate) fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_refuses_absolute_and_malformed_paths_and_keeps_dots_for_locate() {
        assert_eq!(join("a/b", "../c", "a").unwrap(), "a/b/../c");
        assert_eq!(join("a", "/etc", "a"), Err(PathFault::Escape));
        assert_eq!(join("a", "x//y", "a"), Err(PathFault::Invalid));
        assert_eq!(join("a", "x\0y", "a"), Err(PathFault::Invalid));
        assert_eq!(join("", "x", ""), Ok("x".into()));
    }

    #[test]
    fn dotdot_needs_an_existing_directory_and_lookups_ignore_case() {
        let mut s = SourceSet::new();
        s.insert_file("m/files/banner", "B");
        s.insert_link("m/files/Link", "../../outside");
        assert_eq!(
            s.locate("m/files/nonexistent/../banner", "m"),
            Err(LocateFault::Missing)
        );
        assert_eq!(
            s.locate("m/files/banner/../banner", "m"),
            Err(LocateFault::Missing)
        );
        assert!(matches!(
            s.locate("m/files/../files/banner", "m"),
            Ok(Located::File { .. })
        ));
        assert!(
            matches!(s.locate("m/FILES/BANNER", "m"), Ok(Located::File { path, .. }) if path == "m/files/banner")
        );
        // `link` and `Link` are one name to a case-insensitive filesystem, so the link is followed.
        assert_eq!(s.locate("m/files/link", "m"), Err(LocateFault::Escape));
        s.insert_file("m/files/BANNER", "other");
        assert_eq!(s.locate("m/files/banner", "m"), Err(LocateFault::CaseAlias));
        let below: Vec<&str> = s.below("M/Files").map(|(n, _)| n).collect();
        assert_eq!(below.len(), 3, "{below:?}");
    }

    #[test]
    fn dotdot_after_a_symlink_is_resolved_against_the_link_target() {
        let mut s = SourceSet::new();
        s.insert_file("m/real/secret", "S");
        s.insert_file("m/files/x", "X");
        s.insert_link("m/files/link", "../real");
        // Lexically `files/link/../x` is `files/x`; on a real filesystem it is `real/../x`
        // after following the link, which is `m/x`. The resolver must not call that `files/x`.
        assert_eq!(
            s.locate("m/files/link/../x", "m"),
            Err(LocateFault::Missing)
        );
        s.insert_file("m/x", "Y");
        assert!(
            matches!(s.locate("m/files/link/../x", "m"), Ok(Located::File { path, .. }) if path == "m/x")
        );
        assert_eq!(s.locate("m/../outside", "m"), Err(LocateFault::Escape));
    }

    fn set() -> SourceSet {
        let mut s = SourceSet::new();
        s.insert_file("m/files/a", "A");
        s.insert_file("outside", "O");
        s.insert_link("m/files/in", "a");
        s.insert_link("m/files/out", "../../outside");
        s.insert_link("m/files/abs", "/etc/passwd");
        s.insert_link("m/files/loop1", "loop2");
        s.insert_link("m/files/loop2", "loop1");
        s.insert_link("m/files/dangling", "nope");
        s
    }

    #[test]
    fn locate_follows_links_inside_and_refuses_the_rest() {
        let s = set();
        assert!(
            matches!(s.locate("m/files/in", "m"), Ok(Located::File { path, .. }) if path == "m/files/a")
        );
        assert!(matches!(s.locate("m/files", "m"), Ok(Located::Dir { .. })));
        assert_eq!(s.locate("m/files/out", "m"), Err(LocateFault::Escape));
        assert_eq!(s.locate("m/files/abs", "m"), Err(LocateFault::Escape));
        assert_eq!(s.locate("m/files/loop1", "m"), Err(LocateFault::Loop));
        assert_eq!(s.locate("m/files/dangling", "m"), Err(LocateFault::Missing));
        assert_eq!(s.locate("m/files/none", "m"), Err(LocateFault::Missing));
    }

    #[test]
    fn a_directory_link_to_an_ancestor_of_a_root_is_an_escape() {
        let t: toml::Table = "roots = [\"m/files/sub\"]".parse().unwrap();
        let scope = SourceScope::parse("m/bundle.toml", &toml::Value::Table(t)).unwrap();
        let mut s = SourceSet::new();
        s.insert_file("m/files/sub/a", "A");
        s.insert_file("m/files/other", "O");
        s.set_scope(Some(scope));
        assert!(matches!(
            s.locate("m/files/sub", "m"),
            Ok(Located::Dir { .. })
        ));
        assert!(matches!(
            s.locate("m/files/sub/a", "m"),
            Ok(Located::File { .. })
        ));
        for dir in ["m/files", "m"] {
            assert!(
                matches!(
                    s.locate(dir, "m"),
                    Err(LocateFault::Scope(ScopeFault::Outside { .. }))
                ),
                "{dir}"
            );
        }
        // A file that no root holds and that is not beside the bundle is outside too.
        assert!(matches!(
            s.locate("m/files/other", "m"),
            Err(LocateFault::Scope(ScopeFault::Outside { .. }))
        ));
    }

    #[test]
    fn an_empty_directory_is_a_directory_and_is_listed_below_its_parent() {
        let mut s = SourceSet::new();
        s.insert_dir("m/files/empty");
        s.insert_dir("m/files/empty/inner");
        assert!(matches!(
            s.locate("m/files/empty", "m"),
            Ok(Located::Dir { .. })
        ));
        // An ancestor of an empty directory is a directory too.
        assert!(matches!(s.locate("m/files", "m"), Ok(Located::Dir { .. })));
        assert_eq!(s.locate("m/files/none", "m"), Err(LocateFault::Missing));
        let below: Vec<&str> = s.dirs_below("m/files").collect();
        assert_eq!(below, ["empty", "empty/inner"]);
    }

    #[test]
    fn pruned_entries_are_counted_below_a_directory_without_regard_to_case() {
        let mut s = SourceSet::new();
        s.record_pruned("d/.venv");
        s.record_pruned("D/node_modules");
        s.record_pruned("d2/x");
        assert_eq!(s.pruned_under("d"), 2);
        assert_eq!(s.pruned_under("d/.VENV"), 1);
        assert_eq!(s.pruned_under("d3"), 0);
    }
}
