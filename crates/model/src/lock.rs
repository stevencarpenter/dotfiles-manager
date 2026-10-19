//! The source lock (`dotfiles-manager.lock`, docs/architecture.md section 5, "Source lock") and
//! the origin grants (`state/grants.json`, section 8, "Trust grant"). Pure data: parsing takes
//! already-read bytes. The `[manager]` table is parsed by `controls::parse_source_lock`; this
//! module reads the `[[sources]]` entries that `sync` fetches (docs/sync.md).

use crate::digest::sha256_hex;
use crate::fold::fold;
use crate::state::valid_id;
use crate::{Code, Refusal};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Largest lock file (section 5, parser and read limits).
pub const MAX_BYTES: usize = 1 << 20;
pub const SCHEMA: i64 = 1;
/// The `path` of the source that holds the profile and the lock: the checkout `--root` names.
pub const ROOT_PATH: &str = ".";

fn invalid(d: impl Into<String>) -> Refusal {
    Refusal::new(Code::ManifestInvalid, d)
}

fn untrusted(d: impl Into<String>) -> Refusal {
    Refusal::new(Code::SourceUntrusted, d)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockDoc {
    schema: Option<i64>,
    #[serde(default)]
    sources: Vec<LockSource>,
    /// Read by `controls::parse_source_lock` and by `update` (not built); accepted here.
    #[serde(default, rename = "adapters")]
    _adapters: IgnoredAny,
    #[serde(default, rename = "manager")]
    _manager: IgnoredAny,
}

fn root_path() -> String {
    ROOT_PATH.into()
}

/// One `[[sources]]` entry. The root source (`path = "."`) is the tracking source: it holds the
/// lock, so it carries no `commit` and advances to the head of `ref`. A nested source carries
/// an exact `commit`, which must be reachable from `ref`, and lives at `path` inside the root
/// checkout. `tree`, when present, is the recomputed tree digest the checkout must have.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockSource {
    pub id: String,
    pub kind: String,
    pub origin: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub commit: Option<String>,
    pub tree: Option<String>,
    #[serde(default = "root_path")]
    pub path: String,
}

impl LockSource {
    /// Whether this is the root (tracking) source.
    pub fn is_root(&self) -> bool {
        self.path == ROOT_PATH
    }
}

/// The `[[sources]]` of a lock: the root source, then the nested sources by path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lock {
    sources: Vec<LockSource>,
}

impl Lock {
    /// The root (tracking) source, or `None` when the lock declares no source.
    pub fn root(&self) -> Option<&LockSource> {
        self.sources.first()
    }

    /// The nested (exact) sources, by path.
    pub fn nested(&self) -> &[LockSource] {
        self.sources.get(1..).unwrap_or_default()
    }
}

fn hex40(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A full ref name: `refs/` plus components of `[A-Za-z0-9._-]` with no `..`, no empty
/// component, no component that starts with `.` or ends with `.lock`.
pub fn valid_ref(r: &str) -> bool {
    let Some(rest) = r.strip_prefix("refs/") else {
        return false;
    };
    !rest.is_empty()
        && !r.contains("..")
        && rest.split('/').all(|c| {
            !c.is_empty()
                && !c.starts_with('.')
                && !c.ends_with(".lock")
                && c.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
}

/// A nested checkout path: root-relative components with no `.`, `..`, `.git` (any case),
/// empty component, backslash or control character. `.` alone is the root.
pub fn valid_path(p: &str) -> bool {
    p == ROOT_PATH
        || (!p.is_empty()
            && !p.starts_with('/')
            && !p.ends_with('/')
            && p.split('/').all(|c| {
                !c.is_empty()
                    && c != "."
                    && c != ".."
                    && !c.eq_ignore_ascii_case(".git")
                    && !c.chars().any(|ch| ch.is_control() || ch == '\\')
            }))
}

/// The canonical origin form (section 8, S2): scheme `https` or `ssh` (and `file` only when
/// `allow_file`, for the isolated build's local remotes), a lowercase ASCII host, no userinfo
/// except `git` for `ssh`, an absolute path with no trailing `/`, no `.git` suffix, no query
/// and no fragment. Anything else is `E_SOURCE_UNTRUSTED`.
pub fn check_origin(origin: &str, allow_file: bool) -> Result<(), Refusal> {
    // A refused origin is printed with its userinfo masked: a lock from a source could carry a
    // token there, and the message reaches the terminal.
    let bad = |why: &str| {
        untrusted(format!(
            "origin {:?} is not canonical: {why}",
            mask_userinfo(origin)
        ))
    };
    if !origin.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(bad("only printable ASCII is allowed"));
    }
    let (scheme, rest) = origin
        .split_once("://")
        .ok_or_else(|| bad("expected <scheme>://<host>/<path>"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => return Err(bad("the path is missing")),
    };
    match scheme {
        "https" | "ssh" => {}
        "file" if allow_file => {}
        _ => return Err(bad("the scheme must be https or ssh")),
    }
    if scheme == "file" {
        if !authority.is_empty() {
            return Err(bad("a file origin has no host"));
        }
    } else {
        let host = match authority.split_once('@') {
            Some(("git", h)) if scheme == "ssh" => h,
            Some(_) => return Err(bad("userinfo is not allowed")),
            None => authority,
        };
        let (name, port) = host.split_once(':').unwrap_or((host, ""));
        if name.is_empty()
            || name.starts_with('-')
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        {
            return Err(bad("the host must be lowercase ASCII"));
        }
        if host.contains(':') && (port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit())) {
            return Err(bad("the port must be digits"));
        }
    }
    if path.len() < 2 || path.ends_with('/') || path.contains("//") {
        return Err(bad("the path must not be empty, end with / or hold //"));
    }
    if path.contains('?') || path.contains('#') {
        return Err(bad("no query or fragment"));
    }
    if path.ends_with(".git") {
        return Err(bad("no .git suffix"));
    }
    if path.split('/').any(|c| c == "..") {
        return Err(bad("no .. component"));
    }
    Ok(())
}

/// `origin` with any userinfo (the text between `://` and `@` before the first `/`) replaced by
/// `[redacted]`.
fn mask_userinfo(origin: &str) -> String {
    let Some((scheme, rest)) = origin.split_once("://") else {
        return origin.to_string();
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{scheme}://[redacted]{}", &rest[at..]),
        None => origin.to_string(),
    }
}

impl Lock {
    /// Parses the lock and validates its `[[sources]]`. A lock without sources is valid and has
    /// nothing to sync. Exactly one source may be the root (`path = "."`), and it must carry no
    /// `commit`; every nested source must carry one.
    pub fn parse(bytes: &[u8], allow_file: bool) -> Result<Lock, Refusal> {
        if bytes.len() > MAX_BYTES {
            return Err(Refusal::new(
                Code::LimitExceeded,
                "dotfiles-manager.lock exceeds 1 MiB",
            ));
        }
        let text = std::str::from_utf8(bytes).map_err(|_| invalid("the lock is not UTF-8"))?;
        let doc: LockDoc = toml::from_str(text)
            .map_err(|e| invalid(format!("the lock is not valid: {}", e.message())))?;
        if let Some(n) = doc.schema
            && n != SCHEMA
        {
            return Err(Refusal::new(
                Code::SchemaUnsupported,
                format!("lock schema {n} is not supported (supported: {SCHEMA})"),
            ));
        }
        let (mut ids, mut paths) = (BTreeSet::new(), BTreeMap::new());
        let mut roots = 0;
        for s in &doc.sources {
            let at = |why: String| invalid(format!("[[sources]] {}: {why}", s.id));
            if !valid_id(&s.id) {
                return Err(invalid(format!("[[sources]] id {:?} is not valid", s.id)));
            }
            if !ids.insert(s.id.as_str()) {
                return Err(at("duplicate id".into()));
            }
            if s.kind != "git" {
                return Err(at(format!("kind {:?} is not supported; only git", s.kind)));
            }
            check_origin(&s.origin, allow_file)?;
            if !valid_ref(&s.reference) {
                return Err(at(format!("ref {:?} is not a full ref name", s.reference)));
            }
            if let Some(c) = &s.commit
                && !hex40(c)
            {
                return Err(at("commit must be 40 lowercase hex digits".into()));
            }
            if let Some(t) = &s.tree
                && !t.strip_prefix("sha256:").is_some_and(hex64)
            {
                return Err(at("tree must be sha256:<64 hex>".into()));
            }
            if !valid_path(&s.path) {
                return Err(at(format!(
                    "path {:?} is not a plain relative path",
                    s.path
                )));
            }
            // Folded as APFS folds: `base/work` and `base/Work` are one checkout there.
            if paths.insert(fold(&s.path), s.path.as_str()).is_some() {
                return Err(at("duplicate path".into()));
            }
            if s.is_root() {
                roots += 1;
                if s.commit.is_some() {
                    return Err(at(
                        "the root source tracks its ref and carries no commit".into()
                    ));
                }
            } else if s.commit.is_none() {
                return Err(at("a nested source needs an exact commit".into()));
            }
        }
        if !doc.sources.is_empty() && roots != 1 {
            return Err(invalid(
                "[[sources]] needs exactly one root source (path = \".\")",
            ));
        }
        for (a, a_path) in &paths {
            if let Some((_, b_path)) = paths.iter().find(|(b, _)| {
                *b != a
                    && b.strip_prefix(a.as_str())
                        .is_some_and(|r| r.starts_with('/'))
            }) {
                return Err(invalid(format!(
                    "[[sources]] path {b_path:?} lies inside path {a_path:?}"
                )));
            }
        }
        let mut sources = doc.sources;
        sources.sort_by(|a, b| b.is_root().cmp(&a.is_root()).then(a.path.cmp(&b.path)));
        Ok(Lock { sources })
    }
}

/// One trusted origin (section 8, "Trust grant").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub origin: String,
    pub grant_id: String,
    pub granted_at: String,
}

/// `state/grants.json`: the origins the owner trusts, keyed by canonical origin. A state
/// document, so it carries the state schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grants {
    pub schema: i64,
    pub origins: Vec<Grant>,
}

impl Default for Grants {
    fn default() -> Grants {
        Grants {
            schema: crate::state::SCHEMA,
            origins: Vec::new(),
        }
    }
}

impl Grants {
    pub fn parse(bytes: &[u8]) -> Result<Grants, Refusal> {
        crate::state::parse("grants.json", bytes)
    }

    /// Exact string equality of the canonical form.
    pub fn trusts(&self, origin: &str) -> bool {
        self.origins.iter().any(|g| g.origin == origin)
    }

    /// Adds a grant for `origin` unless one exists. Returns the grant id.
    pub fn grant(&mut self, origin: &str, at: &str) -> String {
        if let Some(g) = self.origins.iter().find(|g| g.origin == origin) {
            return g.grant_id.clone();
        }
        let id = format!(
            "grant-{}",
            &sha256_hex(format!("{origin}\n{at}").as_bytes())[..16]
        );
        self.origins.push(Grant {
            origin: origin.into(),
            grant_id: id.clone(),
            granted_at: at.into(),
        });
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = r#"[[sources]]
id = "root"
kind = "git"
origin = "https://example.invalid/org/dotfiles"
ref = "refs/heads/main"
"#;

    #[test]
    fn a_lock_without_sources_is_valid_and_empty() {
        let l = Lock::parse(b"[manager]\ntoolchain = \"1.98.1\"\n", false).unwrap();
        assert!(l.root().is_none());
        assert!(l.nested().is_empty());
    }

    #[test]
    fn the_root_source_tracks_and_nested_sources_pin() {
        let text = format!(
            "{ROOT}\n[[sources]]\nid = \"ext\"\nkind = \"git\"\norigin = \"ssh://git@example.invalid/org/ext\"\nref = \"refs/heads/main\"\ncommit = \"{}\"\npath = \"base/ext\"\n",
            "a".repeat(40)
        );
        let l = Lock::parse(text.as_bytes(), false).unwrap();
        assert_eq!(l.root().unwrap().id, "root");
        assert!(l.root().unwrap().is_root());
        assert_eq!(l.nested()[0].path, "base/ext");

        let pinned_root = format!("{ROOT}commit = \"{}\"\n", "b".repeat(40));
        let e = Lock::parse(pinned_root.as_bytes(), false).unwrap_err();
        assert!(e.detail.contains("carries no commit"), "{e}");

        let floating_nested = format!(
            "{ROOT}\n[[sources]]\nid = \"ext\"\nkind = \"git\"\norigin = \"https://example.invalid/org/ext\"\nref = \"refs/heads/main\"\npath = \"ext\"\n"
        );
        let e = Lock::parse(floating_nested.as_bytes(), false).unwrap_err();
        assert!(e.detail.contains("needs an exact commit"), "{e}");
    }

    #[test]
    fn origins_must_be_canonical() {
        for bad in [
            "https://Example.invalid/org/repo",
            "https://example.invalid/org/repo.git",
            "https://example.invalid/org/repo/",
            "https://user@example.invalid/org/repo",
            "https://example.invalid/org/repo?x=1",
            "git@example.invalid:org/repo",
            "https://-example.invalid/org/repo",
            "http://example.invalid/org/repo",
            "file:///tmp/repo",
        ] {
            assert_eq!(
                check_origin(bad, false).unwrap_err().code,
                Code::SourceUntrusted,
                "{bad}"
            );
        }
        let e = check_origin("https://user:token@example.invalid/org/repo", false).unwrap_err();
        assert!(!e.detail.contains("token"), "{e}");
        assert!(
            e.detail
                .contains("https://[redacted]@example.invalid/org/repo"),
            "{e}"
        );
        check_origin("https://example.invalid/org/repo", false).unwrap();
        check_origin("ssh://git@example.invalid:2222/org/repo", false).unwrap();
        check_origin("file:///tmp/repo", true).unwrap();
    }

    #[test]
    fn refs_and_paths_are_checked() {
        assert!(valid_ref("refs/heads/main"));
        assert!(valid_ref("refs/tags/v1.2"));
        for bad in [
            "main",
            "refs/",
            "refs/heads/..",
            "refs/heads/-x/",
            "refs/heads/a.lock",
        ] {
            assert!(!valid_ref(bad), "{bad}");
        }
        assert!(valid_path("."));
        assert!(valid_path("base/ext"));
        for bad in ["", "/abs", "a/../b", "a/.git/b", "a/", "a\\b"] {
            assert!(!valid_path(bad), "{bad}");
        }
    }

    #[test]
    fn case_aliased_paths_are_one_checkout() {
        let nested = |id: &str, path: &str| {
            format!(
                "\n[[sources]]\nid = \"{id}\"\nkind = \"git\"\norigin = \"https://example.invalid/org/{id}\"\nref = \"refs/heads/main\"\ncommit = \"{}\"\npath = \"{path}\"\n",
                "a".repeat(40)
            )
        };
        let dup = format!(
            "{ROOT}{}{}",
            nested("a", "base/work"),
            nested("b", "base/Work")
        );
        let e = Lock::parse(dup.as_bytes(), false).unwrap_err();
        assert_eq!(e.code, Code::ManifestInvalid);
        assert!(e.detail.contains("b: duplicate path"), "{e}");

        let inside = format!(
            "{ROOT}{}{}",
            nested("a", "base/work"),
            nested("b", "base/Work/inner")
        );
        let e = Lock::parse(inside.as_bytes(), false).unwrap_err();
        assert_eq!(e.code, Code::ManifestInvalid);
        assert!(
            e.detail
                .contains("path \"base/Work/inner\" lies inside path \"base/work\""),
            "{e}"
        );

        let distinct = format!(
            "{ROOT}{}{}",
            nested("a", "base/work"),
            nested("b", "base/works")
        );
        Lock::parse(distinct.as_bytes(), false).unwrap();
    }

    #[test]
    fn grants_round_trip_and_match_exactly() {
        let mut g = Grants::default();
        assert!(!g.trusts("https://example.invalid/org/repo"));
        let id = g.grant("https://example.invalid/org/repo", "2026-10-19T00:00:00Z");
        assert_eq!(
            g.grant("https://example.invalid/org/repo", "2026-10-20T00:00:00Z"),
            id
        );
        assert!(g.trusts("https://example.invalid/org/repo"));
        assert!(!g.trusts("https://example.invalid/org/Repo"));
        let bytes = crate::state::canonical(&g).unwrap();
        assert_eq!(Grants::parse(&bytes).unwrap(), g);
    }
}
