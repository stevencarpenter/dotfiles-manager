//! The `[sources]` declaration of a bundle: which subtrees of the source root are loaded and
//! which paths inside them are skipped. Pure. The CLI loader walks only what this allows, and
//! the resolver uses the same rules to tell a path that was never loaded from a path that is
//! missing (E_SOURCE_ESCAPE against E_SOURCE_MISSING).

use super::error::{ResolveError, category};
use super::source::parent;
use crate::Code;
use crate::fold::fold;
use toml::Value;

const MAX_ENTRIES: usize = 1_000;
const MAX_PATTERN_BYTES: usize = 4_096;

/// Why a path is outside the loaded sources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScopeFault {
    /// An `exclude` pattern matches the path or a directory above it.
    Excluded { pattern: String, path: String },
    /// No declared root holds the path. The text lists the declared roots.
    Outside { roots: String, path: String },
}

/// A bundle's `[sources]` table plus the directory of the bundle that declares it.
///
/// Loaded: every entry under a root, and the files and links that sit directly in the bundle
/// directory. Subdirectories of the bundle directory are loaded only through a root. Patterns
/// are anchored at the source root and compared without regard to case, as source lookup is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceScope {
    roots: Vec<String>,
    exclude: Vec<String>,
    bundle_dir: String,
}

impl SourceScope {
    /// Declared roots, root-relative, in the order written.
    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    /// Declared exclude patterns, in the order written.
    pub fn exclude(&self) -> &[String] {
        &self.exclude
    }

    /// The directory that holds the declaring bundle.
    pub fn bundle_dir(&self) -> &str {
        &self.bundle_dir
    }

    /// The first exclude pattern that matches `path` itself. A pattern that names a directory
    /// excludes its subtree because the loader never descends into a matched directory.
    pub fn excluded_by(&self, path: &str) -> Option<&str> {
        let comps = components(&fold(path));
        self.exclude
            .iter()
            .find(|p| matches_path(&components(&fold(p)), &comps))
            .map(String::as_str)
    }

    /// True when `path` can hold loaded entries: it is under a root, above a root (a directory
    /// that leads to one), above or at the bundle directory, or a direct child of it.
    pub fn reaches(&self, path: &str) -> bool {
        let p = components(&fold(path));
        let bundle = components(&fold(&self.bundle_dir));
        if p.len() <= bundle.len() && bundle[..p.len()] == p[..] {
            return true;
        }
        if p.len() == bundle.len() + 1 && p[..bundle.len()] == bundle[..] {
            return true;
        }
        self.roots.iter().any(|r| {
            let r = components(&fold(r));
            let n = p.len().min(r.len());
            p[..n] == r[..n]
        })
    }

    pub(crate) fn fault(&self, path: &str) -> Option<ScopeFault> {
        if let Some(p) = self.excluded_by(path) {
            return Some(ScopeFault::Excluded {
                pattern: p.to_string(),
                path: path.to_string(),
            });
        }
        (!self.reaches(path)).then(|| ScopeFault::Outside {
            roots: self.roots.join(", "),
            path: path.to_string(),
        })
    }

    /// Reads `[sources]`. `file` is the declaring bundle.
    pub(crate) fn parse(file: &str, v: &Value) -> Result<SourceScope, ResolveError> {
        let invalid = |field: &str, message: String, action: &str| {
            ResolveError::new(Code::ManifestInvalid, file, message, action).field(field)
        };
        let Value::Table(t) = v else {
            return Err(invalid(
                "/sources",
                "`sources` must be a table".into(),
                "use [sources]",
            )
            .rejected(category(v)));
        };
        if let Some(k) = t
            .keys()
            .find(|k| !["roots", "exclude"].contains(&k.as_str()))
        {
            return Err(ResolveError::new(
                Code::FieldUnknown,
                file,
                format!("unknown key `{k}` in /sources; schema 1 accepts known keys only"),
                "remove the key; a new key needs a new schema version",
            )
            .field(format!("/sources/{k}"))
            .rejected(category(&t[k])));
        }
        let list = |key: &str, required: bool| -> Result<Vec<String>, ResolveError> {
            let field = format!("/sources/{key}");
            match t.get(key) {
                None if required => Err(invalid(
                    &field,
                    format!("/sources is missing `{key}`"),
                    "add the list of root-relative directories to load",
                )),
                None => Ok(Vec::new()),
                Some(Value::Array(a)) => {
                    if a.len() > MAX_ENTRIES {
                        return Err(invalid(
                            &field,
                            format!("`{key}` has more than {MAX_ENTRIES} entries"),
                            "merge entries or use a broader pattern",
                        ));
                    }
                    a.iter()
                        .map(|i| match i {
                            Value::String(s) => Ok(s.clone()),
                            other => Err(invalid(
                                &field,
                                format!("`{key}` must be an array of strings"),
                                "use an array of strings",
                            )
                            .rejected(category(other))),
                        })
                        .collect()
                }
                Some(other) => Err(invalid(
                    &field,
                    format!("`{key}` must be an array of strings"),
                    "use an array of strings",
                )
                .rejected(category(other))),
            }
        };
        let roots = list("roots", true)?;
        let exclude = list("exclude", false)?;
        if roots.is_empty() {
            return Err(invalid(
                "/sources/roots",
                "`roots` is empty, so nothing would load".into(),
                "name at least one directory, or remove [sources] to load the whole root",
            ));
        }
        for r in &roots {
            if let Some(why) = path_fault(r, false) {
                return Err(invalid(
                    "/sources/roots",
                    format!("a root {why}"),
                    "use a plain root-relative directory such as `home`",
                )
                .rejected("string"));
            }
        }
        for p in &exclude {
            if let Some(why) = path_fault(p, true) {
                return Err(invalid(
                    "/sources/exclude",
                    format!("an exclude pattern {why}"),
                    "use a root-relative glob such as `**/.venv`",
                )
                .rejected("string"));
            }
        }
        Ok(SourceScope {
            roots,
            exclude,
            bundle_dir: parent(file).to_string(),
        })
    }
}

/// Why `s` is not a normalized root-relative path (or pattern), if it is not.
fn path_fault(s: &str, pattern: bool) -> Option<&'static str> {
    if s.is_empty() || s.len() > MAX_PATTERN_BYTES {
        return Some("is empty or too long");
    }
    if s.starts_with('/') {
        return Some("is absolute");
    }
    if s.chars()
        .any(|c| c.is_control() || super::error::is_invisible(c))
    {
        return Some("has a control or invisible character");
    }
    for c in s.split('/') {
        match c {
            "" => return Some("has an empty component"),
            "." | ".." => return Some("has a `.` or `..` component"),
            _ if !pattern && c.contains(['*', '?']) => return Some("has a wildcard"),
            _ if c.contains("**") && c != "**" => {
                return Some("uses `**` inside a component");
            }
            _ => {}
        }
    }
    None
}

fn components(s: &str) -> Vec<String> {
    s.split('/')
        .filter(|c| !c.is_empty())
        .map(String::from)
        .collect()
}

/// Whole-path glob. `**` as a component matches zero or more components; within a component
/// `*` and `?` follow [`super::compose::glob`]. O(pattern x path) with no backtracking.
fn matches_path(pat: &[String], path: &[String]) -> bool {
    // ok[j]: the pattern prefix handled so far matches path[..j].
    let mut ok = vec![false; path.len() + 1];
    ok[0] = true;
    for p in pat {
        let mut next = vec![false; path.len() + 1];
        if p == "**" {
            let mut seen = false;
            for j in 0..=path.len() {
                seen |= ok[j];
                next[j] = seen;
            }
        } else {
            for j in 0..path.len() {
                next[j + 1] = ok[j] && super::compose::glob(p, &path[j]);
            }
        }
        ok = next;
    }
    ok[path.len()]
}

/// The bundle that `profile` names, as a root-relative path with `.` and `..` collapsed
/// lexically. `None` when the profile does not parse or the path leaves the root. The CLI
/// loader uses it to find `[sources]` before any other file is read; links are resolved there
/// by the operating system and checked against the root, so this is a lookup aid, not a
/// containment decision.
pub fn profile_bundle_path(profile_path: &str, bytes: &[u8]) -> Option<String> {
    let doc = super::docs::profile(profile_path, bytes).ok()?;
    let mut out = components(parent(profile_path));
    for c in doc.bundle.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            c => out.push(c.to_string()),
        }
    }
    (!out.is_empty()).then(|| out.join("/"))
}

/// The `[sources]` table of the bundle at `bundle_path`, if it has one. A bundle that does not
/// parse is refused with the error the resolver would give.
pub fn bundle_scope(bundle_path: &str, bytes: &[u8]) -> Result<Option<SourceScope>, ResolveError> {
    Ok(super::docs::bundle(bundle_path, bytes)?.sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(toml: &str) -> Result<SourceScope, ResolveError> {
        let t: toml::Table = toml.parse().unwrap();
        SourceScope::parse("dfm/bundle.toml", &Value::Table(t))
    }

    #[test]
    fn patterns_are_anchored_and_a_directory_pattern_names_its_subtree_by_prefix() {
        let s = scope(
            r#"roots = ["home", "dfm"]
exclude = ["home/.config/tmux/plugins", "**/__pycache__", "**/.venv", "a/**/z"]"#,
        )
        .unwrap();
        assert_eq!(
            s.excluded_by("home/.config/tmux/plugins"),
            Some("home/.config/tmux/plugins")
        );
        assert_eq!(s.excluded_by("home/.config/tmux/plugins/tpm"), None);
        assert_eq!(
            s.excluded_by("home/x/y/__pycache__"),
            Some("**/__pycache__")
        );
        assert_eq!(s.excluded_by("__PyCache__"), Some("**/__pycache__"));
        assert_eq!(s.excluded_by("__pycache__x"), None);
        assert_eq!(s.excluded_by("__pycache__"), Some("**/__pycache__"));
        assert_eq!(s.excluded_by("a/z"), Some("a/**/z"));
        assert_eq!(s.excluded_by("a/b/c/z"), Some("a/**/z"));
        assert_eq!(s.excluded_by("b/a/z"), None);
        assert_eq!(s.excluded_by("home/.config/tmux"), None);
    }

    #[test]
    fn reach_covers_roots_their_ancestors_and_the_bundle_directory_only_one_level_deep() {
        let s = scope(r#"roots = ["home/.config", "x"]"#).unwrap();
        for p in [
            "home",
            "home/.config",
            "home/.config/git/c",
            "x/y",
            "dfm",
            "dfm/bundle.toml",
        ] {
            assert!(s.reaches(p), "{p}");
        }
        for p in ["home/other", "dfm/modules/core", "modules", "y"] {
            assert!(!s.reaches(p), "{p}");
        }
        assert!(s.reaches("DFM/Bundle.toml"));
    }

    #[test]
    fn the_declaration_is_validated() {
        let bad = |t: &str| scope(t).unwrap_err().code;
        assert_eq!(bad("roots = []"), Code::ManifestInvalid);
        assert_eq!(bad(r#"exclude = ["a"]"#), Code::ManifestInvalid);
        assert_eq!(bad(r#"roots = ["../x"]"#), Code::ManifestInvalid);
        assert_eq!(bad(r#"roots = ["/x"]"#), Code::ManifestInvalid);
        assert_eq!(bad(r#"roots = ["a//b"]"#), Code::ManifestInvalid);
        assert_eq!(bad(r#"roots = ["a/*"]"#), Code::ManifestInvalid);
        assert_eq!(bad(r#"roots = [""]"#), Code::ManifestInvalid);
        assert_eq!(
            bad(r#"roots = ["a"]
exclude = ["a/../b"]"#),
            Code::ManifestInvalid
        );
        assert_eq!(
            bad(r#"roots = ["a"]
exclude = ["a**b"]"#),
            Code::ManifestInvalid
        );
        assert_eq!(bad(r#"roots = [1]"#), Code::ManifestInvalid);
        assert_eq!(
            bad(r#"roots = ["a"]
other = 1"#),
            Code::FieldUnknown
        );
    }

    #[test]
    fn a_profile_names_its_bundle_lexically() {
        let p = b"schema = 1\nid = \"p\"\nbundle = \"../dfm/bundle.toml\"\nplatform = \"macos\"\narchitecture = \"arm64\"\nmodules = []\n";
        assert_eq!(
            profile_bundle_path("profiles/p.toml", p).as_deref(),
            Some("dfm/bundle.toml")
        );
        assert_eq!(profile_bundle_path("p.toml", p), None);
        assert_eq!(profile_bundle_path("p.toml", b"not toml ["), None);
    }
}
