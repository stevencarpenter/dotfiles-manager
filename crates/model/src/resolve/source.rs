//! An in-memory snapshot of trusted, materialized sources. The resolver reads only this value:
//! it never touches the filesystem. Paths are `/`-separated and relative to the snapshot root.
//! Symlinks are kept as entries so containment can be decided without following the real ones.

use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    File(Vec<u8>),
    Link(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceSet {
    entries: BTreeMap<String, Entry>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PathFault {
    /// Absolute, or climbs above the allowed root.
    Escape,
    /// Empty component, control character or NUL.
    Invalid,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LocateFault {
    Escape,
    Missing,
    Loop,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Located<'a> {
    File { path: String, bytes: &'a [u8] },
    Dir { path: String },
}

const MAX_HOPS: usize = 40;

impl SourceSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_file(&mut self, path: impl Into<String>, bytes: impl Into<Vec<u8>>) {
        self.entries.insert(path.into(), Entry::File(bytes.into()));
    }

    pub fn insert_link(&mut self, path: impl Into<String>, target: impl Into<String>) {
        self.entries.insert(path.into(), Entry::Link(target.into()));
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

    fn is_dir(&self, path: &str) -> bool {
        let prefix = format!("{path}/");
        self.entries
            .range(prefix.clone()..)
            .next()
            .is_some_and(|(k, _)| k.starts_with(&prefix))
    }

    /// Entries below `dir`, as paths relative to `dir`.
    pub(crate) fn below<'a>(&'a self, dir: &str) -> impl Iterator<Item = (&'a str, &'a Entry)> {
        let prefix = format!("{dir}/");
        self.entries
            .range(prefix.clone()..)
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .map(move |(k, e)| (&k[dir.len() + 1..], e))
    }

    /// Resolves symlinks inside the snapshot. Every hop must stay under `root`; a link that
    /// leaves it, an absolute link target, or a loop is a fault. `path` is already normalized.
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
                    resolved.pop();
                    continue;
                }
                _ => resolved.push(c),
            }
            let cur = resolved.join("/");
            if let Some(Entry::Link(target)) = self.entries.get(&cur) {
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
        }
        let cur = resolved.join("/");
        match self.entries.get(&cur) {
            Some(Entry::File(bytes)) => Ok(Located::File { path: cur, bytes }),
            Some(Entry::Link(_)) => Err(LocateFault::Missing),
            None if self.is_dir(&cur) => Ok(Located::Dir { path: cur }),
            None => Err(LocateFault::Missing),
        }
    }
}

/// Joins `rel` onto `base_dir` lexically. The result must stay under `root`.
pub(crate) fn join(base_dir: &str, rel: &str, root: &str) -> Result<String, PathFault> {
    if rel.starts_with('/') {
        return Err(PathFault::Escape);
    }
    if rel.is_empty() || rel.chars().any(char::is_control) {
        return Err(PathFault::Invalid);
    }
    let root_len = root.split('/').filter(|c| !c.is_empty()).count();
    let mut parts: Vec<&str> = base_dir.split('/').filter(|c| !c.is_empty()).collect();
    for c in rel.split('/') {
        match c {
            "" => return Err(PathFault::Invalid),
            "." => {}
            ".." => {
                if parts.len() <= root_len {
                    return Err(PathFault::Escape);
                }
                parts.pop();
            }
            _ => parts.push(c),
        }
    }
    Ok(parts.join("/"))
}

pub(crate) fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_stays_under_the_root() {
        assert_eq!(join("a/b", "../c", "a").unwrap(), "a/c");
        assert_eq!(join("a/b", "../../c", "a"), Err(PathFault::Escape));
        assert_eq!(join("a", "/etc", "a"), Err(PathFault::Escape));
        assert_eq!(join("a", "x//y", "a"), Err(PathFault::Invalid));
        assert_eq!(join("a", "x\0y", "a"), Err(PathFault::Invalid));
        assert_eq!(join("", "x", ""), Ok("x".into()));
        assert_eq!(join("", "../x", ""), Err(PathFault::Escape));
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
}
