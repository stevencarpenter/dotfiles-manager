//! Pure resolution of one profile into an effective, attributed composition.
//! No filesystem, process or network access: the caller supplies a [`SourceSet`].

mod docs;
mod error;
mod explain;
mod pointer;
mod schema;
mod scope;
mod source;

pub use error::ResolveError;
pub use explain::explain;
pub use schema::{AdapterCatalog, AdapterSpec, Claims, Field, Include, Schema, TargetRule};
pub use scope::{SourceScope, bundle_scope, profile_bundle_path};
pub use source::{Entry, SourceSet, target_stays_inside};
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
