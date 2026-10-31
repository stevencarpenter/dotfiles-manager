//! The `[sources]` declaration of a bundle: which subtrees of the source root are loaded and
//! which paths inside them are skipped. Pure. The CLI loader walks only what this allows, and
//! the resolver uses the same rules to tell a path that was never loaded from a path that is
//! missing (E_SOURCE_ESCAPE against E_SOURCE_MISSING).

use super::error::{ResolveError, category};
use super::source::{Entry, SourceSet, parent};
use crate::Code;
use crate::fold::fold;
use toml::Value;

const MAX_ENTRIES: usize = 1_000;
const MAX_PATTERN_BYTES: usize = 4_096;
const MAX_BUNDLE_DEPTH: usize = 16;

/// Why a path is outside the loaded sources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScopeFault {
    /// An `exclude` pattern matches the path or a directory above it.
    Excluded { pattern: String, path: String },
    /// No declared root holds the path. The text lists the declared roots.
    Outside { roots: String, path: String },
}

/// A bundle's `[sources]` table plus the bundle that declares it.
///
/// Loaded: every entry under a root, and the files and links that sit directly in the bundle
/// directory. Subdirectories of the bundle directory are loaded only through a root. Patterns
/// are anchored at the source root and compared without regard to case, as source lookup is.
/// Patterns, roots and the bundle directory are folded and split once, when the table is read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceScope {
    roots: Vec<String>,
    exclude: Vec<String>,
    file: String,
    roots_c: Vec<Vec<String>>,
    exclude_c: Vec<Vec<String>>,
    bundle_c: Vec<String>,
}

impl SourceScope {
    /// Declared roots, root-relative, in the order written.
    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    /// The declaring bundle file.
    pub fn file(&self) -> &str {
        &self.file
    }

    /// The directory that holds the declaring bundle.
    pub fn bundle_dir(&self) -> &str {
        parent(&self.file)
    }

    /// The first exclude pattern that matches `path` itself. A pattern that names a directory
    /// excludes its subtree because the loader never descends into a matched directory.
    pub fn excluded_by(&self, path: &str) -> Option<&str> {
        let comps = components(&fold(path));
        self.excluded_comps(&comps)
    }

    fn excluded_comps(&self, comps: &[String]) -> Option<&str> {
        self.exclude_c
            .iter()
            .position(|p| matches_path(p, comps))
            .map(|i| self.exclude[i].as_str())
    }

    /// True when `path` can hold loaded entries: it is under a root, above a root (a directory
    /// that leads to one), above or at the bundle directory, or a direct child of it. Use it
    /// for the steps of a path. The end of a path that names a directory needs [`Self::holds_dir`].
    pub(crate) fn reaches(&self, path: &str) -> bool {
        let p = components(&fold(path));
        let bundle = &self.bundle_c;
        if p.len() <= bundle.len() && bundle[..p.len()] == p[..] {
            return true;
        }
        if p.len() == bundle.len() + 1 && p[..bundle.len()] == bundle[..] {
            return true;
        }
        self.roots_c.iter().any(|r| {
            let n = p.len().min(r.len());
            p[..n] == r[..n]
        })
    }

    /// True when a directory at `path` was loaded as a whole: it is a root or under one. An
    /// ancestor of a root, the bundle directory and a subdirectory beside the bundle are not.
    pub(crate) fn holds_dir(&self, path: &str) -> bool {
        let p = components(&fold(path));
        self.roots_c
            .iter()
            .any(|r| p.len() >= r.len() && p[..r.len()] == r[..])
    }

    pub(crate) fn fault(&self, path: &str) -> Option<ScopeFault> {
        let comps = components(&fold(path));
        if let Some(p) = self.excluded_comps(&comps) {
            return Some(ScopeFault::Excluded {
                pattern: p.to_string(),
                path: path.to_string(),
            });
        }
        (!self.reaches(path)).then(|| self.outside(path))
    }

    pub(crate) fn outside(&self, path: &str) -> ScopeFault {
        ScopeFault::Outside {
            roots: self.roots.join(", "),
            path: path.to_string(),
        }
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
        let scope = SourceScope {
            roots_c: roots.iter().map(|r| components(&fold(r))).collect(),
            exclude_c: exclude.iter().map(|p| components(&fold(p))).collect(),
            bundle_c: components(&fold(parent(file))),
            roots,
            exclude,
            file: file.to_string(),
        };
        // A root at or below an excluded path would be read although the bundle says to skip it.
        for r in &scope.roots {
            let mut prefix = String::new();
            for c in r.split('/') {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(c);
                if let Some(p) = scope.excluded_by(&prefix) {
                    return Err(invalid(
                        "/sources/roots",
                        format!(
                            "a root is at or below a path that the exclude pattern `{p}` skips"
                        ),
                        "narrow the pattern or drop the root",
                    )
                    .rejected("string"));
                }
            }
        }
        // Files beside the bundle are read from its directory, so the directory may not be skipped.
        let mut prefix = String::new();
        for c in scope.bundle_dir().split('/').filter(|c| !c.is_empty()) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(c);
            if let Some(p) = scope.excluded_by(&prefix) {
                return Err(invalid(
                    "/sources/exclude",
                    format!("the bundle directory is at or below a path that the exclude pattern `{p}` skips"),
                    "narrow the pattern: the bundle directory may not be excluded",
                )
                .rejected("string"));
            }
        }
        Ok(scope)
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
            _ if !pattern && c.eq_ignore_ascii_case(".git") => {
                return Some("has a `.git` component");
            }
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

/// The root-relative manifest path of `module`, found through the profile at `profile_path` and
/// the bundles it extends. `core` is the root bundle's `core` key; any other name is the nearest
/// bundle's `[modules]` entry. `None` when no bundle in the chain declares it. Paths are joined
/// and collapsed lexically, as [`profile_bundle_path`] does, so the result is a lookup aid and
/// not a containment decision. A cycle, a chain over 16 bundles, a missing file and a path that
/// leaves the root are refused with the resolver's codes.
pub fn module_manifest(
    sources: &SourceSet,
    profile_path: &str,
    module: &str,
) -> Result<Option<String>, ResolveError> {
    let read = |path: &str, from: &str| -> Result<&[u8], ResolveError> {
        match sources.get(path) {
            Some(Entry::File(b)) => Ok(b),
            _ => Err(ResolveError::new(
                Code::SourceMissing,
                from,
                "the file is not in the loaded sources",
                "check the path and the profile's `bundle` and `extends` keys",
            )),
        }
    };
    let join = |from: &str, rel: &str| -> Result<String, ResolveError> {
        collapse(parent(from), rel).ok_or_else(|| {
            ResolveError::new(
                Code::SourceEscape,
                from,
                "a path leaves the source root",
                "name a path inside the source root",
            )
        })
    };
    let profile = super::docs::profile(profile_path, read(profile_path, profile_path)?)?;
    let mut path = join(profile_path, &profile.bundle)?;
    let mut from = profile_path.to_string();
    let mut seen: Vec<String> = Vec::new();
    loop {
        if seen.contains(&path) {
            return Err(ResolveError::new(
                Code::BundleCycle,
                &from,
                "bundles extend each other in a cycle",
                "remove one `extends` so ancestry is a single chain",
            ));
        }
        if seen.len() >= MAX_BUNDLE_DEPTH {
            return Err(ResolveError::new(
                Code::LimitExceeded,
                &from,
                "bundle ancestry is deeper than 16",
                "flatten the bundle chain",
            ));
        }
        seen.push(path.clone());
        let doc = super::docs::bundle(&path, read(&path, &from)?)?;
        let rel = if module == "core" {
            doc.core.as_ref()
        } else {
            doc.modules.get(module)
        };
        if let Some(rel) = rel {
            return join(&path, rel).map(Some);
        }
        match doc.extends {
            Some(e) => {
                let next = join(&path, &e)?;
                from = path;
                path = next;
            }
            None => return Ok(None),
        }
    }
}

/// `rel` under `base_dir` with `.` and `..` collapsed lexically; `None` when it climbs above
/// the root or is absolute.
fn collapse(base_dir: &str, rel: &str) -> Option<String> {
    // An absolute path leaves the source root, as in `source::join`.
    if rel.starts_with('/') {
        return None;
    }
    let mut out = components(base_dir);
    for c in rel.split('/') {
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

    #[test]
    fn wildcards_stay_inside_a_component_and_double_star_spans_components() {
        let s = scope(
            r#"roots = ["z"]
exclude = ["home/*", "a/?b", "p/**", "**/**/**/x", "m/**/k/*.o"]"#,
        )
        .unwrap();
        assert!(s.excluded_by("home/x").is_some());
        assert!(s.excluded_by("home").is_none());
        assert!(s.excluded_by("home/x/y").is_none());
        assert!(s.excluded_by("a/xb").is_some());
        assert!(s.excluded_by("a/b").is_none());
        assert!(s.excluded_by("p").is_some());
        assert!(s.excluded_by("p/q/r").is_some());
        assert!(s.excluded_by("x").is_some());
        assert!(s.excluded_by("p/q/r/x").is_some());
        assert!(s.excluded_by("q/xx").is_none());
        assert!(s.excluded_by("m/k/a.o").is_some());
        assert!(s.excluded_by("m/1/2/k/a.o").is_some());
        assert!(s.excluded_by("m/1/2/k/a.c").is_none());
    }

    #[test]
    fn a_root_at_or_below_an_excluded_path_is_refused() {
        for (roots, exclude) in [
            ("\"profiles\"", "\"profiles\""),
            ("\"a/b/c\"", "\"a/b\""),
            ("\"a/.venv/x\"", "\"**/.venv\""),
            ("\"home\"", "\"**\""),
        ] {
            let e = scope(&format!("roots = [{roots}]\nexclude = [{exclude}]")).unwrap_err();
            assert_eq!(e.code, Code::ManifestInvalid, "{roots} {exclude}");
        }
        assert!(scope("roots = [\"home\"]\nexclude = [\"home/.venv\"]").is_ok());
    }

    #[test]
    fn the_bundle_directory_may_not_be_excluded() {
        // `scope` declares the bundle in `dfm/`.
        for ex in ["dfm", "**/dfm", "d*"] {
            let e = scope(&format!("roots = [\"mods\"]\nexclude = [\"{ex}\"]")).unwrap_err();
            assert_eq!(e.code, Code::ManifestInvalid, "{ex}");
        }
        assert!(scope("roots = [\"mods\"]\nexclude = [\"dfm/sub\"]").is_ok());
    }

    #[test]
    fn a_root_may_not_hold_a_git_component() {
        for r in ["a/.git", "a/.GIT/b", ".Git"] {
            let e = scope(&format!("roots = [\"{r}\"]")).unwrap_err();
            assert_eq!(e.code, Code::ManifestInvalid, "{r}");
        }
    }

    #[test]
    fn only_a_root_and_its_subtree_count_as_a_loaded_directory() {
        let s = scope("roots = [\"home/sub\", \"dfm\"]").unwrap();
        assert!(s.holds_dir("home/sub"));
        assert!(s.holds_dir("home/sub/x/y"));
        assert!(s.holds_dir("DFM/x"));
        for p in ["home", "", "other", "home/subx"] {
            assert!(!s.holds_dir(p), "{p}");
        }
        // The steps toward a root are still walkable.
        assert!(s.reaches("home"));
    }

    fn tree(files: &[(&str, &str)]) -> super::super::SourceSet {
        let mut s = super::super::SourceSet::new();
        for (p, c) in files {
            s.insert_file(*p, c.as_bytes().to_vec());
        }
        s
    }

    const PROFILE: &str = "schema = 1\nid = \"p\"\nbundle = \"../base/bundle.toml\"\nplatform = \"macos\"\narchitecture = \"arm64\"\nmodules = []\noverrides = []\n";

    #[test]
    fn module_manifest_follows_core_and_the_catalogue() {
        let s = tree(&[
            ("profiles/p.toml", PROFILE),
            (
                "base/bundle.toml",
                "schema = 1\nid = \"b\"\ncore = \"modules/core/module.toml\"\n[modules]\nextra = \"modules/extra/module.toml\"\n",
            ),
        ]);
        let m = |name| module_manifest(&s, "profiles/p.toml", name).unwrap();
        assert_eq!(m("core").as_deref(), Some("base/modules/core/module.toml"));
        assert_eq!(
            m("extra").as_deref(),
            Some("base/modules/extra/module.toml")
        );
        assert_eq!(m("missing"), None);
    }

    #[test]
    fn module_manifest_refuses_an_absolute_path() {
        let s = tree(&[
            ("profiles/p.toml", PROFILE),
            (
                "base/bundle.toml",
                "schema = 1\nid = \"b\"\ncore = \"modules/core/module.toml\"\n[modules]\nextra = \"/modules/extra/module.toml\"\n",
            ),
        ]);
        let e = module_manifest(&s, "profiles/p.toml", "extra").unwrap_err();
        assert_eq!(e.code, Code::SourceEscape);
        let abs = PROFILE.replace("../base/bundle.toml", "/base/bundle.toml");
        let s = tree(&[("profiles/p.toml", &abs)]);
        let e = module_manifest(&s, "profiles/p.toml", "core").unwrap_err();
        assert_eq!(e.code, Code::SourceEscape);
    }

    #[test]
    fn module_manifest_follows_extends_two_levels() {
        let s = tree(&[
            ("profiles/p.toml", PROFILE),
            (
                "base/bundle.toml",
                "schema = 1\nid = \"mid\"\nextends = \"../root/bundle.toml\"\n[modules]\nmine = \"m/module.toml\"\n",
            ),
            (
                "root/bundle.toml",
                "schema = 1\nid = \"root\"\ncore = \"c/module.toml\"\n[modules]\nold = \"o/module.toml\"\n",
            ),
        ]);
        let m = |name| module_manifest(&s, "profiles/p.toml", name).unwrap();
        assert_eq!(m("mine").as_deref(), Some("base/m/module.toml"));
        assert_eq!(m("old").as_deref(), Some("root/o/module.toml"));
        assert_eq!(m("core").as_deref(), Some("root/c/module.toml"));
        assert_eq!(m("none"), None);
    }

    #[test]
    fn module_manifest_refuses_a_cycle_a_missing_file_and_an_escape() {
        let cyc = tree(&[
            ("profiles/p.toml", PROFILE),
            (
                "base/bundle.toml",
                "schema = 1\nid = \"a\"\nextends = \"bundle.toml\"\n[modules]\n",
            ),
        ]);
        let e = module_manifest(&cyc, "profiles/p.toml", "x").unwrap_err();
        assert_eq!(e.code, Code::BundleCycle);
        let none = tree(&[("profiles/p.toml", PROFILE)]);
        let e = module_manifest(&none, "profiles/p.toml", "x").unwrap_err();
        assert_eq!(e.code, Code::SourceMissing);
        let esc = tree(&[(
            "p.toml",
            "schema = 1\nid = \"p\"\nbundle = \"../b.toml\"\nplatform = \"macos\"\narchitecture = \"arm64\"\nmodules = []\noverrides = []\n",
        )]);
        let e = module_manifest(&esc, "p.toml", "x").unwrap_err();
        assert_eq!(e.code, Code::SourceEscape);
    }
}
