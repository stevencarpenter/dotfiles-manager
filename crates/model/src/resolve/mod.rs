//! Pure resolution of one profile into an effective, attributed composition.
//! No filesystem, process or network access: the caller supplies a [`SourceSet`].

mod docs;
mod error;
mod explain;
mod pointer;
mod schema;
mod source;

pub use error::ResolveError;
pub use explain::explain;
pub use schema::{AdapterCatalog, AdapterSpec, Claims, Field, Schema, TargetRule};
pub use source::{Entry, SourceSet};
mod compose;

use std::collections::BTreeMap;
use toml::Value;

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
    pub native_document: bool,
    /// One entry per leaf of `value`, keyed by RFC 6901 pointer.
    pub provenance: BTreeMap<String, Provenance>,
    /// Output paths this resource claims.
    pub claims: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    pub profile: String,
    pub platform: String,
    pub architecture: String,
    /// Core first, then each module after its prerequisites, lexically among the ready ones.
    pub modules: Vec<String>,
    pub resources: BTreeMap<String, ResolvedResource>,
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
