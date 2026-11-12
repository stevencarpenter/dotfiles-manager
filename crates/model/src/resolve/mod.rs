//! Pure resolution of one profile into an effective, attributed composition.
//! No filesystem, process or network access: the caller supplies a [`SourceSet`].

mod docs;
mod error;
mod explain;
pub(crate) mod pointer;
mod schema;
mod scope;
mod source;

pub(crate) use docs::PROFILE_SCHEMA;
#[cfg(test)]
pub(crate) use docs::valid_id;
pub use error::ResolveError;
pub use explain::explain;
pub use schema::{AdapterCatalog, AdapterSpec, Claims, Field, Include, Schema, TargetRule};
pub use scope::{SourceScope, bundle_scope, module_manifest, profile_bundle_path};
pub use source::{Entry, SourceSet, is_git_name, target_stays_inside};
mod compose;
pub(crate) use compose::glob;

use std::collections::BTreeMap;
pub use toml::{Table, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Override,
    Contribution,
}

/// Where one effective value came from. `owner` declared the resource, `changer` and `reason`
/// are set when a permitted change supplied the value. `source_file` is the manifest that
/// declared the effective value, `source_path` the resolved source file when the value is a
/// source reference, and `content_id` the digest of that source. Secret-bearing resources carry
/// no content identity.
#[derive(Clone, Debug, PartialEq)]
pub struct Provenance {
    pub owner: String,
    pub original: Option<Value>,
    pub change: Option<ChangeKind>,
    pub changer: Option<String>,
    pub reason: Option<String>,
    pub source_file: String,
    pub source_path: Option<String>,
    pub content_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedResource {
    pub id: String,
    pub module: String,
    pub declared_in: String,
    pub kind: String,
    pub adapter: String,
    pub target: Option<String>,
    pub value: Value,
    pub defaults: Vec<String>,
    pub extensions: Vec<String>,
    pub secret_values: bool,
    /// The output can hold credentials, so no content hash is recorded for it.
    pub sensitive: bool,
    pub native_document: bool,
    /// One entry per leaf of `value`, keyed by RFC 6901 pointer.
    pub provenance: BTreeMap<String, Provenance>,
    /// Output paths this resource claims.
    pub claims: Vec<String>,
    /// Directories this resource reads fragments from, claimed as a prefix.
    pub dir_claims: Vec<DirClaim>,
    /// How the application orders competing values in this native document, when known.
    pub precedence: Option<String>,
}

/// Each package adapter and the pointer of the source it fetches from. That source needs no grant
/// only when the bound checkout declared it ([`owner_declared`], docs/sync.md, "Source
/// identities"); otherwise its identity needs one. `None`: the adapter installs from its
/// provider's registry, or its source always needs a grant (`script/v1`). The cli test
/// `every_package_adapter_states_its_source_rule` fails for a package adapter not listed here.
pub const SOURCE_ADAPTERS: &[(&str, Option<&str>)] = &[
    ("homebrew/v1", None),
    ("mise/v1", Some("/name")),
    ("apt/v1", None),
    ("uv-tool/v1", Some("/source")),
    ("git-checkout/v1", Some("/url")),
    ("script/v1", None),
];

/// The [`SOURCE_ADAPTERS`] pointer of `adapter`, or `None`.
pub fn source_pointer(adapter: &str) -> Option<&'static str> {
    SOURCE_ADAPTERS
        .iter()
        .find(|(a, _)| *a == adapter)
        .and_then(|(_, p)| *p)
}

/// Whether the owner's own checkout declared `r` and the value at `pointer`, so its source needs
/// no grant (docs/sync.md, "Source identities"). True only when the declaring manifest and the
/// manifest that supplied the value both lie outside every nested source path of the lock.
/// `nested` holds those paths relative to the bound root; the paths are compared by whole
/// components after the case fold. A value without a provenance entry is not owner declared.
pub fn owner_declared(r: &ResolvedResource, pointer: &str, nested: &[String]) -> bool {
    let outside = |file: &str| {
        let file = crate::fold::fold(file);
        nested.iter().all(|n| {
            let n = crate::fold::fold(n);
            file != n
                && !file
                    .strip_prefix(&n)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    };
    r.provenance
        .get(pointer)
        .is_some_and(|p| outside(&r.declared_in) && outside(&p.source_file))
}

/// A fragment directory: only files matching `pattern` may be placed in it by another resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirClaim {
    pub dir: String,
    pub pattern: String,
}

/// One include directive of an authored native document, with the resource that declares the
/// included file (if any resource does).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncludeEntry {
    /// 1-based position in the order the application reads the document.
    pub position: usize,
    pub path: String,
    pub condition: Option<String>,
    pub resource: Option<String>,
    pub module: Option<String>,
}

/// The ordered includes of one authored native document (for example a Git config).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncludeGroup {
    pub base: String,
    pub base_target: Option<String>,
    pub entries: Vec<IncludeEntry>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    pub profile: String,
    pub platform: String,
    pub architecture: String,
    /// Core first, then each module after its prerequisites, lexically among the ready ones.
    pub modules: Vec<String>,
    pub resources: BTreeMap<String, ResolvedResource>,
    /// The profile's updaters, in declared order (docs/upgrade.md).
    pub updaters: Vec<crate::updater::Updater>,
    /// Native documents whose include order is declared by an authored source file.
    pub includes: Vec<IncludeGroup>,
}

/// Resolves the profile at `profile_path` (a path inside `sources`) against the adapters in
/// `catalog`. Pure: the same inputs always give the same result or the same first error.
pub fn resolve(
    sources: &SourceSet,
    profile_path: &str,
    catalog: &AdapterCatalog,
) -> Result<Resolved, ResolveError> {
    compose::resolve(sources, profile_path, catalog)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(declared_in: &str, url_from: Option<&str>) -> ResolvedResource {
        let provenance = url_from
            .map(|f| {
                let p = Provenance {
                    owner: "m".into(),
                    original: None,
                    change: None,
                    changer: None,
                    reason: None,
                    source_file: f.into(),
                    source_path: None,
                    content_id: None,
                };
                BTreeMap::from([("/url".to_string(), p)])
            })
            .unwrap_or_default();
        ResolvedResource {
            id: "m.r".into(),
            module: "m".into(),
            declared_in: declared_in.into(),
            kind: "package".into(),
            adapter: "git-checkout/v1".into(),
            target: None,
            value: Value::Table(Table::new()),
            defaults: Vec::new(),
            extensions: Vec::new(),
            secret_values: false,
            sensitive: false,
            native_document: false,
            provenance,
            claims: Vec::new(),
            dir_claims: Vec::new(),
            precedence: None,
        }
    }

    fn nested() -> Vec<String> {
        vec!["base/ext".into()]
    }

    #[test]
    fn a_root_declaration_is_owner_declared() {
        let m = "base/modules/m/module.toml";
        assert!(owner_declared(&res(m, Some(m)), "/url", &nested()));
        assert!(owner_declared(&res(m, Some(m)), "/url", &[]));
    }

    #[test]
    fn a_nested_declaration_is_not() {
        let m = "base/ext/modules/w/module.toml";
        assert!(!owner_declared(&res(m, Some(m)), "/url", &nested()));
        // The case fold: APFS opens one directory for both spellings.
        let m = "base/Ext/modules/w/module.toml";
        assert!(!owner_declared(&res(m, Some(m)), "/url", &nested()));
    }

    #[test]
    fn a_root_declaration_with_a_value_from_a_nested_overlay_is_not() {
        let r = res(
            "base/modules/m/module.toml",
            Some("base/ext/modules/w/module.toml"),
        );
        assert!(!owner_declared(&r, "/url", &nested()));
    }

    #[test]
    fn a_missing_provenance_entry_is_not() {
        let m = "base/modules/m/module.toml";
        assert!(!owner_declared(&res(m, None), "/url", &nested()));
        assert!(!owner_declared(&res(m, Some(m)), "/source", &nested()));
    }

    #[test]
    fn a_path_that_only_shares_a_prefix_is_outside() {
        let m = "base/extra/modules/m/module.toml";
        assert!(owner_declared(&res(m, Some(m)), "/url", &nested()));
    }
}
