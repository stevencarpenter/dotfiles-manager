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
use super::schema::{self, AdapterCatalog, AdapterSpec, Claims, Fault, Found, TargetRule};
use super::source::{self, LocateFault, Located, PathFault, SourceSet};
use super::{ChangeKind, Provenance, Resolved, ResolvedResource};
use crate::Code;
use crate::digest::{EntryKind, TreeEntry, sha256_hex, tree_digest};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use toml::Value;

const MAX_BUNDLE_DEPTH: usize = 16;
const MAX_RESOURCES: usize = 10_000;

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
            "a symlink in the path leaves the trusted root",
            "replace the link with a file inside the source, or remove it",
        ),
        LocateFault::Loop => ResolveError::new(
            Code::SourceEscape,
            from,
            "symlinks in the path form a loop or are nested too deeply",
            "remove the link loop",
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
    let resolved = finish(sources, catalog, &resources, &effective)?;

    // Phase 5: output claims.
    check_claims(&resolved)?;

    Ok(Resolved {
        profile: profile.id,
        platform: profile.platform,
        architecture: profile.architecture,
        modules: order,
        resources: resolved,
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
    let mut queue: VecDeque<String> = profile.modules.iter().cloned().collect();
    queue.extend(core.requires.iter().cloned());
    let mut modules: BTreeMap<String, ModuleDoc> = BTreeMap::new();
    modules.insert("core".into(), core);
    while let Some(id) = queue.pop_front() {
        if modules.contains_key(&id) {
            continue;
        }
        let Some(entry) = catalogue.get(&id) else {
            continue;
        };
        let field = pointer::join("/modules", &id);
        let (path, bytes) = l.read_path(&entry.bundle, &field, &entry.path, &entry.bundle_dir)?;
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
        queue.extend(m.requires.iter().cloned());
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
    // Kahn's algorithm: core first, then the lexically smallest module whose prerequisites
    // have all been placed. The order is for presentation only.
    let mut order = vec!["core".to_string()];
    let mut left: BTreeSet<&String> = modules.keys().filter(|k| *k != "core").collect();
    while !left.is_empty() {
        let next = left
            .iter()
            .find(|id| {
                modules[**id]
                    .requires
                    .iter()
                    .all(|r| r == "core" || order.contains(r))
            })
            .map(|id| (*id).clone());
        match next {
            Some(id) => {
                left.remove(&id);
                order.push(id);
            }
            None => {
                let names: Vec<&str> = left.iter().map(|s| s.as_str()).collect();
                return Err(ResolveError::new(
                    Code::DependencyCycle,
                    &modules[*left.iter().next().expect("nonempty")].file,
                    format!(
                        "modules depend on each other in a cycle: {}",
                        names.join(", ")
                    ),
                    "remove one `requires` entry to break the cycle",
                )
                .field("/requires"));
            }
        }
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
            if !owner.doc.defaults.contains(&o.path) {
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
                    format!("the override must be a {}", category(base)),
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
            if !owner.doc.extensions.contains(&c.path) {
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
    roots
        .and_then(|r| {
            r.iter()
                .find(|(root, _)| ptr == root.as_str() || ptr.starts_with(&format!("{root}/")))
                .map(|(_, s)| s.clone())
        })
        .unwrap_or_else(|| Supplier {
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
) -> Result<BTreeMap<String, ResolvedResource>, ResolveError> {
    let mut out = BTreeMap::new();
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
                    format!("expected a {expected}"),
                    "supply a value of the declared type; boolean and integer are different types",
                )
                .rejected(found),
                Fault::MissingKey => at(
                    Code::ManifestInvalid,
                    "a required key is missing".into(),
                    "add the key",
                ),
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
        // Source containment, through symlinks, relative to the file that supplied the value.
        let mut prov: BTreeMap<String, Provenance> = BTreeMap::new();
        let mut srcs: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
        for s in &found.sources {
            let sup = supplier_for(owner, roots, &s.pointer);
            let (path, id) = contain(sources, &sup, gid, &s.pointer, &s.rel, spec)?;
            srcs.insert(s.pointer.clone(), (path, id));
        }
        let base_tokens = |p: &str| {
            pointer::parse(p)
                .ok()
                .and_then(|t| pointer::get(&r.value, &t).cloned())
        };
        let mut ls = Vec::new();
        leaves(value, "", &mut ls);
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
                claims: claims_of(spec, owner, gid, value)?,
            },
        );
    }
    Ok(out)
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
) -> Result<(String, Option<String>), ResolveError> {
    let wrap = |e: ResolveError| e.resource(gid).field(ptr);
    let path =
        source::join(&sup.dir, rel, &sup.dir).map_err(|f| wrap(path_error(&sup.file, ptr, f)))?;
    match sources
        .locate(&path, &sup.dir)
        .map_err(|f| wrap(locate_error(&sup.file, ptr, f)))?
    {
        Located::File { path, bytes } => {
            let id = (!spec.secret_values).then(|| format!("sha256:{}", sha256_hex(bytes)));
            Ok((path, id))
        }
        Located::Dir { path } => {
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
            let id = (!spec.secret_values).then(|| format!("sha256:{}", tree_digest(entries)));
            Ok((path, id))
        }
    }
}

fn claims_of(
    spec: &AdapterSpec,
    owner: &Owned,
    gid: &str,
    value: &Value,
) -> Result<Vec<String>, ResolveError> {
    Ok(match spec.claims {
        Claims::None => Vec::new(),
        Claims::Target => owner.doc.target.iter().cloned().collect(),
        Claims::Leaves {
            roots,
            entries,
            enabled,
        } => {
            let get = |p: &str| pointer::get(value, &pointer::parse(p).expect("static pointer"));
            let (Some(Value::Array(roots)), Some(Value::Table(entries))) =
                (get(roots), get(entries))
            else {
                return Ok(Vec::new());
            };
            let mut out = Vec::new();
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
            out
        }
    })
}

/// Phase 5: two resources may not claim one output, compared case-insensitively because the
/// target filesystem may fold case, and a claimed file may not also be a parent of another claim.
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
                return Err(collision(r, gid, claim, other, other_claim));
            }
        }
    }
    for (gid, r) in resolved {
        for claim in &r.claims {
            let folded = fold(claim);
            let mut prefix = String::new();
            for part in folded.split('/').take(folded.split('/').count() - 1) {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(part);
                if let Some((other, other_claim)) = owners.get(&prefix) {
                    return Err(collision(r, gid, claim, other, other_claim));
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
) -> ResolveError {
    ResolveError::new(
        Code::TargetCollision,
        &r.declared_in,
        format!("output `{claim}` collides with `{other_claim}` (compared without case, and a file cannot also be a directory)"),
        "give each resource its own output path",
    )
    .resource(gid)
    .field("/target")
    .rejected("string")
    .conflicts_with(other)
}
