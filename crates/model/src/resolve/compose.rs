//! The five validation phases, in the order DESIGN.md section 9 fixes:
//! 1 document and bundle schema, 2 selection and dependency validity, 3 composition authority and
//! conflicts, 4 resolved adapter schema and source containment, 5 output claims.
//! The first error wins. Every collection is a `BTreeMap`/`BTreeSet` or sorted, so the result
//! and the first error do not depend on declaration order. Order never grants authority.

use super::docs::{
    self, BundleDoc, ContributionDecl, ModuleDoc, OverrideDecl, ProfileDoc, ResourceDoc,
};
use super::error::{ResolveError, category};
use super::pointer;
use super::schema::{self, AdapterCatalog, AdapterSpec, Claims, Fault, Found, Include, TargetRule};
use super::source::{self, LocateFault, Located, PathFault, SourceSet};
use super::{
    ChangeKind, DirClaim, IncludeEntry, IncludeGroup, Provenance, Resolved, ResolvedResource,
};
use crate::Code;
use crate::digest::{EntryKind, TreeEntry, sha256_hex, tree_digest};
use std::collections::{BTreeMap, BTreeSet};
use toml::Value;

const MAX_BUNDLE_DEPTH: usize = 16;
const MAX_RESOURCES: usize = 10_000;
const MAX_MODULES: usize = 10_000;
const MAX_TOTAL_DOC_BYTES: usize = 64 << 20;
/// Leaves across the whole composition. Each leaf gets a provenance record.
const MAX_LEAVES: usize = 200_000;
/// Include directives in one authored native document, and in all of them together (counted
/// once per resource that links the document).
const MAX_INCLUDES_PER_DOC: usize = 1_000;
/// Output claims one resource may enumerate, and all resources together. A distribution adapter
/// claims roots x entries paths, so the product is checked before any claim is built.
const MAX_CLAIMS_PER_RESOURCE: usize = 100_000;
const MAX_CLAIMS_TOTAL: usize = 200_000;
const MAX_INCLUDES_TOTAL: usize = 10_000;

fn limit(file: &str, what: &str) -> ResolveError {
    ResolveError::new(
        Code::LimitExceeded,
        file,
        what.to_string(),
        "split the composition or remove unused modules",
    )
}

struct Loaded<'a> {
    sources: &'a SourceSet,
}

impl<'a> Loaded<'a> {
    /// Reads a manifest. `from` is the declaring file, `root` the directory the path must stay in.
    fn read(
        &self,
        from: &str,
        field: &str,
        rel: &str,
        root: &str,
    ) -> Result<(String, &'a [u8]), ResolveError> {
        let base = source::parent(from);
        let path = source::join(base, rel, root).map_err(|f| path_error(from, field, f))?;
        self.read_path(from, field, &path, root)
    }

    /// Reads an already-normalized path, following symlinks that stay under `root`.
    fn read_path(
        &self,
        from: &str,
        field: &str,
        path: &str,
        root: &str,
    ) -> Result<(String, &'a [u8]), ResolveError> {
        match self.sources.locate(path, root) {
            Ok(Located::File { path, bytes }) => Ok((path, bytes)),
            Ok(Located::Dir { .. }) => Err(ResolveError::new(
                Code::ReferenceInvalid,
                from,
                "the reference names a directory, not a document",
                "point it at a document",
            )
            .field(field)),
            Err(f) => Err(locate_error(from, field, f)),
        }
    }
}

fn path_error(from: &str, field: &str, f: PathFault) -> ResolveError {
    match f {
        PathFault::Escape => ResolveError::new(
            Code::SourceEscape,
            from,
            "the path leaves the trusted root of its declaring source",
            "use a relative path that stays inside the declaring source",
        ),
        PathFault::Invalid => ResolveError::new(
            Code::ManifestInvalid,
            from,
            "the path has an empty component or a control character",
            "use a plain relative path",
        ),
    }
    .field(field)
    .rejected("string")
}

fn locate_error(from: &str, field: &str, f: LocateFault) -> ResolveError {
    match f {
        LocateFault::Missing => ResolveError::new(
            Code::SourceMissing,
            from,
            "the referenced source is not in the materialized, trusted sources",
            "fetch or add the source, or fix the path",
        ),
        LocateFault::Escape => ResolveError::new(
            Code::SourceEscape,
            from,
            "the path or a symlink in it leaves the trusted root",
            "use a path that stays inside the declaring source, and replace or remove any link that leaves it",
        ),
        LocateFault::Loop => ResolveError::new(
            Code::SourceEscape,
            from,
            "symlinks in the path form a loop or are nested too deeply",
            "remove the link loop",
        ),
        LocateFault::CaseAlias => ResolveError::new(
            Code::SourceEscape,
            from,
            "two source entries differ only by case, so a case-insensitive filesystem cannot tell them apart",
            "rename one of them",
        ),
    }
    .field(field)
    .rejected("string")
}

/// Owner of the effective value at a pointer: where it was declared and by whom.
#[derive(Clone)]
struct Supplier {
    file: String,
    dir: String,
    changer: String,
    change: Option<ChangeKind>,
    reason: Option<String>,
}

struct Change<'a> {
    changer: String,
    file: String,
    dir: String,
    over: Option<&'a OverrideDecl>,
    contrib: Option<&'a ContributionDecl>,
}

struct Owned {
    module: String,
    file: String,
    dir: String,
    doc: ResourceDoc,
    defaults: BTreeSet<String>,
    extensions: BTreeSet<String>,
}

pub(crate) fn resolve(
    sources: &SourceSet,
    profile_path: &str,
    catalog: &AdapterCatalog,
) -> Result<Resolved, ResolveError> {
    let l = Loaded { sources };

    // Phase 1: documents.
    let (pfile, pbytes) = l.read_path(profile_path, "profile", profile_path, "")?;
    let profile = docs::profile(&pfile, pbytes)?;
    let chain = bundle_chain(&l, &profile)?;
    let catalogue = catalogue(&chain)?;
    let modules = load_modules(&l, &chain, &catalogue, &profile)?;

    // Phase 2: selection and dependency validity.
    let order = select(&profile, &catalogue, &modules)?;

    // Phase 3: authority and conflicts.
    let mut resources: BTreeMap<String, Owned> = BTreeMap::new();
    for m in order.iter().map(|id| &modules[id]) {
        for r in &m.resources {
            resources.insert(
                format!("{}.{}", m.id, r.id),
                Owned {
                    module: m.id.clone(),
                    file: m.file.clone(),
                    dir: source::parent(&m.file).to_string(),
                    defaults: r.defaults.iter().cloned().collect(),
                    extensions: r.extensions.iter().cloned().collect(),
                    doc: r.clone(),
                },
            );
        }
    }
    if resources.len() > MAX_RESOURCES {
        return Err(ResolveError::new(
            Code::LimitExceeded,
            &profile.file,
            "the composition has more than 10000 resources",
            "split the composition",
        ));
    }
    let changes = collect_changes(&order, &modules, &profile);
    let effective = authorize(&resources, &changes)?;

    // Phase 4: resolved adapter schema and source containment.
    let (resolved, includes) = finish(sources, catalog, &resources, &effective)?;

    // Phase 5: output claims.
    check_claims(&resolved)?;

    Ok(Resolved {
        profile: profile.id,
        platform: profile.platform,
        architecture: profile.architecture,
        modules: order,
        resources: resolved,
        includes,
    })
}

fn bundle_chain(l: &Loaded, profile: &ProfileDoc) -> Result<Vec<BundleDoc>, ResolveError> {
    let mut chain: Vec<BundleDoc> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut from = profile.file.clone();
    let mut field = "/bundle".to_string();
    let mut rel = profile.bundle.clone();
    loop {
        // A reference between bundles may name any bundle in the materialized sources.
        let (path, bytes) = l.read(&from, &field, &rel, "")?;
        if !seen.insert(path.clone()) {
            return Err(ResolveError::new(
                Code::BundleCycle,
                &from,
                "bundles extend each other in a cycle",
                "remove one `extends` so ancestry is a single chain",
            )
            .field(field));
        }
        if chain.len() >= MAX_BUNDLE_DEPTH {
            return Err(ResolveError::new(
                Code::LimitExceeded,
                &from,
                "bundle ancestry is deeper than 16",
                "flatten the bundle chain",
            ));
        }
        let b = docs::bundle(&path, bytes)?;
        if let Some(prev) = chain.iter().find(|p| p.id == b.id) {
            return Err(ResolveError::new(
                Code::ManifestInvalid,
                &path,
                "two bundles in one ancestry share an ID",
                "give each bundle a unique ID",
            )
            .field("/id")
            .conflicts_with(prev.file.clone()));
        }
        let next = b.extends.clone();
        chain.push(b);
        match next {
            Some(e) => {
                from = path;
                field = "/extends".into();
                rel = e;
            }
            None => return Ok(chain),
        }
    }
}

struct CatalogueEntry {
    path: String,
    bundle_dir: String,
    bundle: String,
}

fn catalogue(chain: &[BundleDoc]) -> Result<BTreeMap<String, CatalogueEntry>, ResolveError> {
    let mut map: BTreeMap<String, CatalogueEntry> = BTreeMap::new();
    for b in chain.iter().rev() {
        let dir = source::parent(&b.file);
        for (id, rel) in &b.modules {
            let field = pointer::join("/modules", id);
            if id == "core" {
                return Err(ResolveError::new(
                    Code::CoreReplacement,
                    &b.file,
                    "the catalogue cannot define a module named core",
                    "the core module comes only from the root bundle `core` key",
                )
                .field(field));
            }
            if let Some(prev) = map.get(id) {
                return Err(ResolveError::new(
                    Code::ModuleDuplicate,
                    &b.file,
                    format!("module `{id}` is already defined by an inherited bundle"),
                    "an extending bundle may add modules, not redefine inherited ones",
                )
                .field(field)
                .conflicts_with(prev.bundle.clone()));
            }
            let path = source::join(dir, rel, dir).map_err(|f| path_error(&b.file, &field, f))?;
            map.insert(
                id.clone(),
                CatalogueEntry {
                    path,
                    bundle_dir: dir.to_string(),
                    bundle: b.file.clone(),
                },
            );
        }
    }
    Ok(map)
}

/// Loads core and every module reachable from the selection through `requires`. A name that is
/// not in the catalogue is left for phase 2 to report.
fn load_modules(
    l: &Loaded,
    chain: &[BundleDoc],
    catalogue: &BTreeMap<String, CatalogueEntry>,
    profile: &ProfileDoc,
) -> Result<BTreeMap<String, ModuleDoc>, ResolveError> {
    let root = chain.last().expect("a chain has at least one bundle");
    let core_rel = root.core.as_deref().expect("a root bundle declares a core");
    let root_dir = source::parent(&root.file);
    let (path, bytes) = l.read(&root.file, "/core", core_rel, root_dir)?;
    let core = docs::module(&path, bytes)?;
    if core.id != "core" {
        return Err(ResolveError::new(
            Code::ReferenceInvalid,
            &root.file,
            "`core` must reference the core module",
            "point `core` at a module whose id is `core`",
        )
        .field("/core"));
    }
    // A sorted frontier: modules load in ID order, so which malformed document is reported
    // first does not depend on how the profile or `requires` lists are ordered.
    let mut frontier: BTreeSet<String> = profile.modules.iter().cloned().collect();
    frontier.extend(core.requires.iter().cloned());
    let mut modules: BTreeMap<String, ModuleDoc> = BTreeMap::new();
    modules.insert("core".into(), core);
    let mut total_bytes = 0usize;
    while let Some(id) = frontier.pop_first() {
        if modules.contains_key(&id) {
            continue;
        }
        let Some(entry) = catalogue.get(&id) else {
            continue;
        };
        if modules.len() >= MAX_MODULES {
            return Err(limit(
                &entry.bundle,
                "more than 10000 modules are reachable",
            ));
        }
        let field = pointer::join("/modules", &id);
        let (path, bytes) = l.read_path(&entry.bundle, &field, &entry.path, &entry.bundle_dir)?;
        total_bytes += bytes.len();
        if total_bytes > MAX_TOTAL_DOC_BYTES {
            return Err(limit(
                &entry.bundle,
                "module documents exceed 64 MiB in total",
            ));
        }
        let m = docs::module(&path, bytes)?;
        if m.id != id {
            return Err(ResolveError::new(
                Code::ReferenceInvalid,
                &entry.bundle,
                format!("catalogue key `{id}` references a module with a different ID"),
                "make the catalogue key equal the module id",
            )
            .field(field));
        }
        frontier.extend(
            m.requires
                .iter()
                .filter(|r| !modules.contains_key(*r))
                .cloned(),
        );
        modules.insert(id, m);
    }
    Ok(modules)
}

fn select(
    profile: &ProfileDoc,
    catalogue: &BTreeMap<String, CatalogueEntry>,
    modules: &BTreeMap<String, ModuleDoc>,
) -> Result<Vec<String>, ResolveError> {
    let mut seen = BTreeSet::new();
    for (i, m) in profile.modules.iter().enumerate() {
        let field = format!("/modules/{i}");
        if m == "core" || !seen.insert(m.as_str()) {
            return Err(ResolveError::new(
                Code::ModuleDuplicate,
                &profile.file,
                format!("module `{m}` is selected more than once; core is always selected"),
                "list each non-core module once",
            )
            .field(field)
            .rejected("string"));
        }
        if !catalogue.contains_key(m) {
            return Err(unknown_module(&profile.file, &field, m));
        }
    }
    let core = &modules["core"];
    if !core.requires.is_empty() {
        return Err(ResolveError::new(
            Code::ManifestInvalid,
            &core.file,
            "the core cannot depend on other modules",
            "remove `requires` from the core",
        )
        .field("/requires"));
    }
    for m in modules.values() {
        let mut seen = BTreeSet::new();
        for (i, r) in m.requires.iter().enumerate() {
            let field = format!("/requires/{i}");
            if r == "core" || !seen.insert(r.as_str()) {
                return Err(ResolveError::new(
                    Code::ModuleDuplicate,
                    &m.file,
                    format!("`{r}` is required more than once; core is always required"),
                    "list each prerequisite once and omit core",
                )
                .field(field)
                .rejected("string"));
            }
            if !catalogue.contains_key(r) {
                return Err(unknown_module(&m.file, &field, r));
            }
        }
    }
    for m in modules.values() {
        if !m.platforms.contains(&profile.platform) {
            return Err(ResolveError::new(
                Code::PlatformUnsupported,
                &m.file,
                format!("module `{}` does not support the profile platform", m.id),
                "select a different module or profile platform",
            )
            .field("/platforms")
            .rejected("string")
            .conflicts_with(profile.file.clone()));
        }
    }
    // Kahn's algorithm with in-degree counts and an ordered ready set: core first, then the
    // lexically smallest module whose prerequisites are all placed. For presentation only.
    let mut waiting: BTreeMap<&str, usize> = BTreeMap::new();
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for m in modules.values().filter(|m| m.id != "core") {
        waiting.insert(m.id.as_str(), m.requires.len());
        for r in &m.requires {
            dependents
                .entry(r.as_str())
                .or_default()
                .push(m.id.as_str());
        }
    }
    let mut order = vec!["core".to_string()];
    let mut ready: BTreeSet<&str> = waiting
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(id, _)| *id)
        .collect();
    while let Some(id) = ready.pop_first() {
        order.push(id.to_string());
        waiting.remove(id);
        for d in dependents.get(id).into_iter().flatten() {
            if let Some(n) = waiting.get_mut(d) {
                *n -= 1;
                if *n == 0 {
                    ready.insert(d);
                }
            }
        }
    }
    if let Some((first, _)) = waiting.iter().next() {
        let names: Vec<&str> = waiting.keys().copied().collect();
        return Err(ResolveError::new(
            Code::DependencyCycle,
            &modules[*first].file,
            format!(
                "modules depend on each other in a cycle: {}",
                names.join(", ")
            ),
            "remove one `requires` entry to break the cycle",
        )
        .field("/requires"));
    }
    Ok(order)
}

fn unknown_module(file: &str, field: &str, id: &str) -> ResolveError {
    ResolveError::new(
        Code::ModuleUnknown,
        file,
        format!("module `{id}` is not in the bundle catalogue"),
        "add it to a bundle catalogue or remove the reference",
    )
    .field(field)
    .rejected("string")
}

fn collect_changes<'a>(
    order: &[String],
    modules: &'a BTreeMap<String, ModuleDoc>,
    profile: &'a ProfileDoc,
) -> Vec<Change<'a>> {
    let mut out = Vec::new();
    for id in order {
        let m = &modules[id];
        let dir = source::parent(&m.file).to_string();
        for o in &m.overrides {
            out.push(Change {
                changer: m.id.clone(),
                file: m.file.clone(),
                dir: dir.clone(),
                over: Some(o),
                contrib: None,
            });
        }
        for c in &m.contributions {
            out.push(Change {
                changer: m.id.clone(),
                file: m.file.clone(),
                dir: dir.clone(),
                over: None,
                contrib: Some(c),
            });
        }
    }
    for o in &profile.overrides {
        out.push(Change {
            changer: profile.id.clone(),
            file: profile.file.clone(),
            dir: source::parent(&profile.file).to_string(),
            over: Some(o),
            contrib: None,
        });
    }
    // Validation and conflict reporting walk a sorted view, so declaration order cannot choose
    // which problem is reported.
    out.sort_by(|a, b| {
        let key = |c: &Change| {
            let (r, p) = c
                .over
                .map(|o| (&o.resource, &o.path))
                .or_else(|| c.contrib.map(|x| (&x.resource, &x.path)))
                .expect("a change is an override or a contribution");
            (
                r.clone(),
                p.clone(),
                c.contrib.map(|x| x.key.clone()),
                c.changer.clone(),
            )
        };
        key(a).cmp(&key(b))
    });
    out
}

/// What phase 3 produced: each resource's value with permitted changes applied, and where
/// each applied change root points.
struct Effective {
    values: BTreeMap<String, Value>,
    roots: BTreeMap<String, BTreeMap<String, Supplier>>,
}

fn unknown_resource(ch: &Change, resource: &str, field: &str) -> ResolveError {
    ResolveError::new(
        Code::ReferenceInvalid,
        &ch.file,
        format!("resource `{resource}` is not in the selected composition"),
        "select the module that declares it, or correct the resource ID",
    )
    .resource(resource)
    .field(field)
    .rejected("string")
}

fn authorize(
    resources: &BTreeMap<String, Owned>,
    changes: &[Change],
) -> Result<Effective, ResolveError> {
    let mut eff = Effective {
        values: resources
            .iter()
            .map(|(k, v)| (k.clone(), v.doc.value.clone()))
            .collect(),
        roots: BTreeMap::new(),
    };
    // Validate each change on its own, then look for competing owners.
    let mut claimed: BTreeMap<(String, String), &Change> = BTreeMap::new();
    for ch in changes {
        if let Some(o) = ch.over {
            let owner = resources
                .get(&o.resource)
                .ok_or_else(|| unknown_resource(ch, &o.resource, "resource"))?;
            let tokens = pointer::parse(&o.path).map_err(|f| {
                ResolveError::new(
                    Code::ReferenceInvalid,
                    &ch.file,
                    format!("override path is invalid: {}", f.describe()),
                    "use an RFC 6901 pointer listed in the owner's defaults",
                )
                .resource(&o.resource)
                .field(&o.path)
            })?;
            if !owner.defaults.contains(&o.path) {
                return Err(ResolveError::new(
                    Code::OverrideForbidden,
                    &ch.file,
                    "the path is protected: the owner does not list it in policy.defaults",
                    format!("ask the owner of `{}` to expose the path as a default, or drop the override", o.resource),
                )
                .resource(&o.resource)
                .field(&o.path)
                .rejected(category(&o.value))
                .conflicts_with(owner.module.clone()));
            }
            let base = pointer::get(&owner.doc.value, &tokens).expect("policy paths exist");
            if std::mem::discriminant(base) != std::mem::discriminant(&o.value) {
                return Err(ResolveError::new(
                    Code::TypeMismatch,
                    &ch.file,
                    format!("the override must be of type {}", category(base)),
                    "supply a value of the owner's type; boolean and integer are different types",
                )
                .resource(&o.resource)
                .field(&o.path)
                .rejected(category(&o.value)));
            }
            if let Some(prev) = claimed.insert((o.resource.clone(), o.path.clone()), ch) {
                return Err(ResolveError::new(
                    Code::OverrideConflict,
                    &ch.file,
                    "two overrides address the same resource and path, even if their values are equal",
                    "keep one override; the winner is never chosen by order",
                )
                .resource(&o.resource)
                .field(&o.path)
                .rejected(category(&o.value))
                .conflicts_with(format!("{} ({})", prev.changer, prev.file)));
            }
        } else if let Some(c) = ch.contrib {
            let owner = resources
                .get(&c.resource)
                .ok_or_else(|| unknown_resource(ch, &c.resource, "resource"))?;
            pointer::parse(&c.path).map_err(|f| {
                ResolveError::new(
                    Code::ReferenceInvalid,
                    &ch.file,
                    format!("contribution path is invalid: {}", f.describe()),
                    "use an RFC 6901 pointer listed in the owner's extensions",
                )
                .resource(&c.resource)
                .field(&c.path)
            })?;
            if !owner.extensions.contains(&c.path) {
                return Err(ResolveError::new(
                    Code::OverrideForbidden,
                    &ch.file,
                    "the path is not an exposed extension map",
                    format!(
                        "ask the owner of `{}` to list the map in policy.extensions",
                        c.resource
                    ),
                )
                .resource(&c.resource)
                .field(&c.path)
                .rejected(category(&c.value))
                .conflicts_with(owner.module.clone()));
            }
            let prefix = format!("{}.", ch.changer);
            let rest = c.key.strip_prefix(&prefix).unwrap_or("");
            let valid_rest = !rest.is_empty()
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
                && rest.as_bytes()[0].is_ascii_alphanumeric();
            if !valid_rest {
                return Err(ResolveError::new(
                    Code::ExtensionNamespace,
                    &ch.file,
                    format!("a contribution key must be `{}` followed by a name of lowercase letters, digits, `.`, `_` or `-`", prefix),
                    format!("rename the key to start with `{prefix}`"),
                )
                .resource(&c.resource)
                .field(format!("{}/{}", c.path, pointer::escape(&c.key)))
                .rejected("string"));
            }
            let tokens = pointer::parse(&c.path).expect("parsed above");
            let exists = match pointer::get(&owner.doc.value, &tokens) {
                Some(Value::Table(m)) => m.contains_key(&c.key),
                _ => false,
            };
            if exists {
                return Err(ResolveError::new(
                    Code::ExtensionConflict,
                    &ch.file,
                    "the key already exists in the owner's map and cannot be replaced by a contribution",
                    "choose another key; true substitution needs an exposed default",
                )
                .resource(&c.resource)
                .field(format!("{}/{}", c.path, pointer::escape(&c.key)))
                .rejected(category(&c.value))
                .conflicts_with(owner.module.clone()));
            }
            let slot = (
                c.resource.clone(),
                format!("{}/{}", c.path, pointer::escape(&c.key)),
            );
            if let Some(prev) = claimed.insert(slot, ch) {
                return Err(ResolveError::new(
                    Code::ExtensionConflict,
                    &ch.file,
                    "two contributors claim one key",
                    "give each contributor its own namespaced key",
                )
                .resource(&c.resource)
                .field(format!("{}/{}", c.path, pointer::escape(&c.key)))
                .rejected(category(&c.value))
                .conflicts_with(format!("{} ({})", prev.changer, prev.file)));
            }
        }
    }
    // Apply. Every address is distinct, so the order of application cannot matter.
    for ch in changes {
        let supplier = |kind, reason: &str| Supplier {
            file: ch.file.clone(),
            dir: ch.dir.clone(),
            changer: ch.changer.clone(),
            change: Some(kind),
            reason: Some(reason.to_string()),
        };
        if let Some(o) = ch.over {
            let tokens = pointer::parse(&o.path).expect("validated");
            let v = eff.values.get_mut(&o.resource).expect("validated");
            assert!(pointer::replace(v, &tokens, o.value.clone()));
            eff.roots
                .entry(o.resource.clone())
                .or_default()
                .insert(o.path.clone(), supplier(ChangeKind::Override, &o.reason));
        } else if let Some(c) = ch.contrib {
            let tokens = pointer::parse(&c.path).expect("validated");
            let v = eff.values.get_mut(&c.resource).expect("validated");
            if let Some(Value::Table(m)) = pointer::get_mut(v, &tokens) {
                m.insert(c.key.clone(), c.value.clone());
            }
            eff.roots.entry(c.resource.clone()).or_default().insert(
                format!("{}/{}", c.path, pointer::escape(&c.key)),
                supplier(ChangeKind::Contribution, &c.reason),
            );
        }
    }
    Ok(eff)
}

/// The declaring source for the value at `ptr`: the change that covers it, or the owner.
fn supplier_for(owner: &Owned, roots: Option<&BTreeMap<String, Supplier>>, ptr: &str) -> Supplier {
    // The change that covers `ptr` is at `ptr` itself or at one of its ancestors, so look each
    // up directly instead of scanning every change root.
    let covering = roots.and_then(|r| {
        let mut at = ptr;
        loop {
            if let Some(s) = r.get(at) {
                return Some(s);
            }
            at = &at[..at.rfind('/')?];
        }
    });
    covering.cloned().unwrap_or_else(|| Supplier {
        file: owner.file.clone(),
        dir: owner.dir.clone(),
        changer: owner.module.clone(),
        change: None,
        reason: None,
    })
}

fn leaves<'v>(v: &'v Value, ptr: &str, out: &mut Vec<(String, &'v Value)>) {
    match v {
        Value::Table(t) if !t.is_empty() => {
            for (k, child) in t {
                leaves(child, &pointer::join(ptr, k), out);
            }
        }
        _ => out.push((ptr.to_string(), v)),
    }
}

fn finish(
    sources: &SourceSet,
    catalog: &AdapterCatalog,
    resources: &BTreeMap<String, Owned>,
    eff: &Effective,
) -> Result<(BTreeMap<String, ResolvedResource>, Vec<IncludeGroup>), ResolveError> {
    let mut out = BTreeMap::new();
    let mut raw_includes: Vec<(String, Vec<Include>)> = Vec::new();
    // Each authored document is parsed once, however many resources link it.
    let mut include_cache: BTreeMap<String, Vec<Include>> = BTreeMap::new();
    let mut include_total = 0usize;
    let mut leaf_budget = 0usize;
    let mut claim_budget = 0usize;
    // One digest per located source path, however many references name it.
    let mut digests: BTreeMap<String, String> = BTreeMap::new();
    for (gid, owner) in resources {
        let r = &owner.doc;
        let spec = catalog.get(&r.adapter).ok_or_else(|| {
            ResolveError::new(
                Code::AdapterUnknown,
                &owner.file,
                format!("adapter `{}` is not built into this manager", r.adapter),
                "use a supported versioned adapter identifier",
            )
            .resource(gid)
            .field("/adapter")
            .rejected("string")
        })?;
        if spec.kind != r.kind {
            return Err(ResolveError::new(
                Code::AdapterUnknown,
                &owner.file,
                format!(
                    "adapter `{}` does not implement kind `{}`",
                    r.adapter, r.kind
                ),
                format!(
                    "use kind `{}` with this adapter, or a different adapter",
                    spec.kind
                ),
            )
            .resource(gid)
            .field("/kind")
            .rejected("string"));
        }
        let value = &eff.values[gid];
        let roots = eff.roots.get(gid);
        let mut found = Found::default();
        if let Err(f) = schema::validate(value, &spec.value, "", &mut found) {
            let sup = supplier_for(owner, roots, &f.pointer);
            let at = |code, message: String, action: &str| {
                ResolveError::new(code, &sup.file, message, action)
                    .resource(gid)
                    .field(f.pointer.clone())
            };
            return Err(match f.fault {
                Fault::UnknownKey { found } => at(
                    Code::FieldUnknown,
                    format!("unknown key; adapter `{}` accepts known keys only", spec.id),
                    "remove the key; a new key needs a new schema version",
                )
                .rejected(found),
                Fault::Type { expected, found } => at(
                    Code::TypeMismatch,
                    format!("expected type {expected}"),
                    "supply a value of the declared type; boolean and integer are different types",
                )
                .rejected(found),
                Fault::MissingKey => at(
                    Code::ManifestInvalid,
                    "a required key is missing".into(),
                    "add the key",
                ),
                Fault::OutOfRange(why) => at(
                    Code::ManifestInvalid,
                    why.into(),
                    "use a mode such as 0o600 (private) or 0o644",
                )
                .rejected("integer"),
            });
        }
        for h in &found.homes {
            if let Some(why) = schema::home_path_fault(&h.path) {
                return Err(target_error(
                    &supplier_for(owner, roots, &h.pointer).file,
                    gid,
                    &h.pointer,
                    why,
                ));
            }
        }
        match (spec.target, &r.target) {
            (TargetRule::Required, None) => {
                return Err(ResolveError::new(
                    Code::ManifestInvalid,
                    &owner.file,
                    format!("adapter `{}` needs a `target`", spec.id),
                    "add a HOME-relative target",
                )
                .resource(gid)
                .field("/target"));
            }
            (TargetRule::Forbidden, Some(_)) => {
                return Err(ResolveError::new(
                    Code::ManifestInvalid,
                    &owner.file,
                    format!(
                        "adapter `{}` enumerates its own claims and takes no `target`",
                        spec.id
                    ),
                    "remove the target",
                )
                .resource(gid)
                .field("/target")
                .rejected("string"));
            }
            (_, Some(t)) => {
                if let Some(why) = schema::home_path_fault(t) {
                    return Err(target_error(&owner.file, gid, "/target", why));
                }
            }
            _ => {}
        }
        // Claims are counted before anything expensive is done for this resource.
        let (claims, dir_claims) = claims_of(spec, owner, gid, value, &mut claim_budget)?;
        // Source containment, through symlinks, relative to the file that supplied the value.
        let mut prov: BTreeMap<String, Provenance> = BTreeMap::new();
        let mut srcs: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
        for s in &found.sources {
            let sup = supplier_for(owner, roots, &s.pointer);
            let (path, id) = contain(sources, &sup, gid, &s.pointer, &s.rel, spec, &mut digests)?;
            srcs.insert(s.pointer.clone(), (path, id));
        }
        let base_tokens = |p: &str| {
            pointer::parse(p)
                .ok()
                .and_then(|t| pointer::get(&r.value, &t).cloned())
        };
        let mut ls = Vec::new();
        leaves(value, "", &mut ls);
        leaf_budget += ls.len();
        if leaf_budget > MAX_LEAVES {
            return Err(limit(
                &owner.file,
                "the composition has more than 200000 values",
            ));
        }
        for (ptr, _) in ls {
            let sup = supplier_for(owner, roots, &ptr);
            let (source_path, content_id) = match srcs.get(&ptr) {
                Some((p, c)) => (Some(p.clone()), c.clone()),
                None => (None, None),
            };
            prov.insert(
                ptr.clone(),
                Provenance {
                    owner: owner.module.clone(),
                    original: base_tokens(&ptr),
                    change: sup.change,
                    changer: sup.change.map(|_| sup.changer.clone()),
                    reason: sup.reason.clone(),
                    source_file: sup.file,
                    source_path,
                    content_id,
                },
            );
        }
        if let (Some(parse), Some((path, _))) = (spec.source_includes, srcs.get("/source")) {
            // The source is the authored native document; its include lines fix the order.
            let key = format!("{}\0{path}", spec.id);
            if !include_cache.contains_key(&key) {
                let parsed = match sources.locate(path, "") {
                    Ok(Located::File { bytes, .. }) => {
                        std::str::from_utf8(bytes).map(parse).unwrap_or_default()
                    }
                    _ => Vec::new(),
                };
                if parsed.len() > MAX_INCLUDES_PER_DOC {
                    return Err(limit(
                        &owner.file,
                        "a native document has more than 1000 include directives",
                    ));
                }
                include_cache.insert(key.clone(), parsed);
            }
            let found = &include_cache[&key];
            include_total += found.len();
            if include_total > MAX_INCLUDES_TOTAL {
                return Err(limit(
                    &owner.file,
                    "the composition has more than 10000 include directives",
                ));
            }
            if !found.is_empty() {
                raw_includes.push((gid.clone(), found.clone()));
            }
        }
        out.insert(
            gid.clone(),
            ResolvedResource {
                id: gid.clone(),
                module: owner.module.clone(),
                declared_in: owner.file.clone(),
                kind: r.kind.clone(),
                adapter: r.adapter.clone(),
                target: r.target.clone(),
                value: value.clone(),
                defaults: r.defaults.clone(),
                extensions: r.extensions.clone(),
                secret_values: spec.secret_values,
                native_document: spec.native_document,
                provenance: prov,
                claims,
                dir_claims,
                precedence: spec.precedence.map(String::from),
            },
        );
    }
    let groups = include_groups(&out, raw_includes);
    Ok((out, groups))
}

/// Orders each authored native document's includes and ties each to the resource that
/// declares the included file. Matching is by output path, without regard to case.
fn include_groups(
    resolved: &BTreeMap<String, ResolvedResource>,
    raw: Vec<(String, Vec<Include>)>,
) -> Vec<IncludeGroup> {
    let by_target: BTreeMap<String, &ResolvedResource> = resolved
        .values()
        .filter_map(|r| r.target.as_ref().map(|t| (t.to_lowercase(), r)))
        .collect();
    raw.into_iter()
        .map(|(base, includes)| {
            let entries: Vec<IncludeEntry> = includes
                .into_iter()
                .enumerate()
                .map(|(i, inc)| {
                    let hit = inc
                        .path
                        .strip_prefix("~/")
                        .and_then(|rel| by_target.get(&rel.to_lowercase()))
                        .filter(|r| r.id != base);
                    IncludeEntry {
                        position: i + 1,
                        path: inc.path,
                        condition: inc.condition,
                        resource: hit.map(|r| r.id.clone()),
                        module: hit.map(|r| r.module.clone()),
                    }
                })
                .collect();
            IncludeGroup {
                base_target: resolved[&base].target.clone(),
                entries,
                base,
            }
        })
        .collect()
}

fn target_error(file: &str, gid: &str, field: &str, why: &str) -> ResolveError {
    ResolveError::new(
        Code::TargetInvalid,
        file,
        format!("unsafe output path: {why}"),
        "use a HOME-relative path without `.`, `..`, empty components, `~` or control characters",
    )
    .resource(gid)
    .field(field)
    .rejected("string")
}

/// Checks one source path and returns its snapshot path plus a content identity. A directory
/// source is checked entry by entry, so a link inside it cannot leave the root either.
fn contain(
    sources: &SourceSet,
    sup: &Supplier,
    gid: &str,
    ptr: &str,
    rel: &str,
    spec: &AdapterSpec,
    digests: &mut BTreeMap<String, String>,
) -> Result<(String, Option<String>), ResolveError> {
    let wrap = |e: ResolveError| e.resource(gid).field(ptr);
    let path =
        source::join(&sup.dir, rel, &sup.dir).map_err(|f| wrap(path_error(&sup.file, ptr, f)))?;
    match sources
        .locate(&path, &sup.dir)
        .map_err(|f| wrap(locate_error(&sup.file, ptr, f)))?
    {
        Located::File { path, bytes } => {
            let id = (!spec.secret_values).then(|| {
                digests
                    .entry(path.clone())
                    .or_insert_with(|| format!("sha256:{}", sha256_hex(bytes)))
                    .clone()
            });
            Ok((path, id))
        }
        Located::Dir { path } => {
            // A directory reached again was already checked entry by entry, and its digest is
            // the same, so do not walk and hash it again for every reference.
            // The key includes the root: the same directory can be reached from two roots, and
            // entry links must stay inside each of them.
            let key = format!("{}\0{path}", sup.dir);
            if let Some(id) = digests.get(&key) {
                return Ok((path.clone(), (!spec.secret_values).then(|| id.clone())));
            }
            let mut entries = Vec::new();
            for (name, e) in sources.below(&path) {
                let full = format!("{path}/{name}");
                match sources.locate(&full, &sup.dir) {
                    Ok(Located::File { bytes, .. }) => {
                        if let super::source::Entry::File(_) = e {
                            entries.push(TreeEntry {
                                path: name.to_string(),
                                kind: EntryKind::File {
                                    exec: false,
                                    sha256: sha256_hex(bytes),
                                },
                            });
                        } else if let super::source::Entry::Link(t) = e {
                            entries.push(TreeEntry {
                                path: name.to_string(),
                                kind: EntryKind::Symlink { target: t.clone() },
                            });
                        }
                    }
                    Ok(Located::Dir { .. }) => {
                        if let super::source::Entry::Link(t) = e {
                            entries.push(TreeEntry {
                                path: name.to_string(),
                                kind: EntryKind::Symlink { target: t.clone() },
                            });
                        }
                    }
                    Err(f) => return Err(wrap(locate_error(&sup.file, ptr, f))),
                }
            }
            let id = format!("sha256:{}", tree_digest(entries));
            digests.insert(key, id.clone());
            Ok((path, (!spec.secret_values).then_some(id)))
        }
    }
}

fn claims_of(
    spec: &AdapterSpec,
    owner: &Owned,
    gid: &str,
    value: &Value,
    budget: &mut usize,
) -> Result<(Vec<String>, Vec<DirClaim>), ResolveError> {
    let get = |p: &str| pointer::get(value, &pointer::parse(p).expect("static pointer"));
    Ok(match spec.claims {
        Claims::None => (Vec::new(), Vec::new()),
        Claims::Target => (owner.doc.target.iter().cloned().collect(), Vec::new()),
        Claims::TargetAndFragments { dir, pattern } => {
            let (Some(Value::String(d)), Some(Value::String(p))) = (get(dir), get(pattern)) else {
                return Ok((owner.doc.target.iter().cloned().collect(), Vec::new()));
            };
            if p.is_empty() || p.chars().any(|c| "/[]{}\\".contains(c)) {
                return Err(ResolveError::new(
                    Code::ManifestInvalid,
                    &owner.file,
                    "the fragment pattern may use only `*` and `?` and must name one path component",
                    "use a pattern such as `*.md`",
                )
                .resource(gid)
                .field(pattern)
                .rejected("string"));
            }
            (
                owner.doc.target.iter().cloned().collect(),
                vec![DirClaim {
                    dir: d.clone(),
                    pattern: p.clone(),
                }],
            )
        }
        Claims::Leaves {
            roots,
            entries,
            enabled,
        } => {
            let (Some(Value::Array(roots)), Some(Value::Table(entries))) =
                (get(roots), get(entries))
            else {
                return Ok((Vec::new(), Vec::new()));
            };
            // Refuse before building the list: roots x entries can be enormous.
            let count = roots.len().saturating_mul(entries.len());
            *budget = budget.saturating_add(count);
            if count > MAX_CLAIMS_PER_RESOURCE || *budget > MAX_CLAIMS_TOTAL {
                return Err(
                    limit(&owner.file, "the composition claims too many output paths")
                        .resource(gid)
                        .field("/entries"),
                );
            }
            let mut out = Vec::with_capacity(count);
            for root in roots.iter().filter_map(Value::as_str) {
                for (name, e) in entries {
                    if schema::home_path_fault(name).is_some() || name.contains('/') {
                        return Err(target_error(
                            &owner.file,
                            gid,
                            "/entries",
                            "entry name is not a single safe component",
                        ));
                    }
                    if !matches!(e.get(enabled), Some(Value::Boolean(false))) {
                        out.push(format!("{root}/{name}"));
                    }
                }
            }
            (out, Vec::new())
        }
    })
}

/// `*` matches any run of characters and `?` any one, within a single path component.
pub(crate) fn glob(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    let (mut pi, mut ni, mut star, mut mark) = (0, 0, None, 0);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Phase 5: two resources may not claim one output, compared case-insensitively because the
/// target filesystem may fold case, and a claimed file may not also be the parent of another
/// claim. A fragment directory is claimed as a prefix: only files whose name matches the
/// owner's pattern may be placed directly in it, and only by another resource.
fn check_claims(resolved: &BTreeMap<String, ResolvedResource>) -> Result<(), ResolveError> {
    let fold = |p: &str| {
        p.split('/')
            .map(str::to_lowercase)
            .collect::<Vec<_>>()
            .join("/")
    };
    let mut owners: BTreeMap<String, (&str, &str)> = BTreeMap::new();
    for (gid, r) in resolved {
        for claim in &r.claims {
            if let Some((other, other_claim)) =
                owners.insert(fold(claim), (gid.as_str(), claim.as_str()))
            {
                return Err(collision(
                    r,
                    gid,
                    claim,
                    other,
                    other_claim,
                    "the same output path, compared without case",
                ));
            }
        }
    }
    // Directory claims may not equal or nest inside each other. Sorted token lists put every
    // descendant right after its ancestor, so neighbours are enough.
    let mut dirs: Vec<(Vec<String>, &str, &DirClaim)> = Vec::new();
    for (gid, r) in resolved {
        for d in &r.dir_claims {
            let tokens = fold(&d.dir).split('/').map(String::from).collect();
            dirs.push((tokens, gid.as_str(), d));
        }
    }
    dirs.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    for pair in dirs.windows(2) {
        if pair[1].0.starts_with(&pair[0].0) {
            return Err(collision(
                &resolved[pair[1].1],
                pair[1].1,
                &pair[1].2.dir,
                pair[0].1,
                &pair[0].2.dir,
                "fragment directories may not be equal or nested",
            ));
        }
    }
    // A fragment directory may not sit below a path that another resource claims as a file.
    for (tokens, gid, d) in &dirs {
        for end in 1..tokens.len() {
            if let Some((other, other_claim)) = owners.get(&tokens[..end].join("/")) {
                return Err(collision(
                    &resolved[*gid],
                    gid,
                    &d.dir,
                    other,
                    other_claim,
                    "a file cannot also be the parent of a fragment directory",
                ));
            }
        }
    }
    let dir_by_path: BTreeMap<String, (&str, &DirClaim)> = dirs
        .iter()
        .map(|(t, gid, d)| (t.join("/"), (*gid, *d)))
        .collect();
    for (gid, r) in resolved {
        for claim in &r.claims {
            let folded = fold(claim);
            let parts: Vec<&str> = folded.split('/').collect();
            // The claim itself, then each proper prefix.
            for end in (1..=parts.len()).rev() {
                let prefix = parts[..end].join("/");
                if let Some(&(owner, d)) = dir_by_path.get(&prefix) {
                    let rest = &parts[end..];
                    let as_fragment = owner != gid.as_str()
                        && rest.len() == 1
                        && glob(&d.pattern.to_lowercase(), rest[0]);
                    if !as_fragment {
                        return Err(collision(
                            r,
                            gid,
                            claim,
                            owner,
                            &d.dir,
                            &if owner == gid.as_str() {
                                "an assembled file cannot be placed in the directory it reads fragments from".to_string()
                            } else {
                                format!(
                                    "that directory is read as fragments, so another resource may place only a file matching `{}` directly in it",
                                    d.pattern
                                )
                            },
                        ));
                    }
                }
                if end < parts.len() {
                    // Ancestors that are files, not directories.
                    if let Some((other, other_claim)) = owners.get(&prefix) {
                        return Err(collision(
                            r,
                            gid,
                            claim,
                            other,
                            other_claim,
                            "a file cannot also be a directory",
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn collision(
    r: &ResolvedResource,
    gid: &str,
    claim: &str,
    other: &str,
    other_claim: &str,
    why: &str,
) -> ResolveError {
    ResolveError::new(
        Code::TargetCollision,
        &r.declared_in,
        format!("output `{claim}` collides with `{other_claim}`: {why}"),
        "give each resource its own output path",
    )
    .resource(gid)
    .field("/target")
    .rejected("string")
    .conflicts_with(other)
}
