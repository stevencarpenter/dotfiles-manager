//! Plan construction: claims, parent topology, ownership, producer and consumer edges.

use super::graph;
use super::{
    Action, Binary, Blocker, Fragment, Incomplete, Input, LinkTo, Node, Observations, Operation,
    Ownership, Plan, Precondition, Request, Seen, Source, Status, link_fingerprint, node_state,
};
use crate::digest::sha256_hex;
use crate::fold::fold;
use crate::refusal::{Code, Refusal};
use crate::resolve::{DirClaim, Resolved, ResolvedResource, glob};
use crate::state::Class;
use std::collections::{BTreeMap, BTreeSet};
use toml::Value;

/// The kinds a configuration plan covers. Packages and applications are provisioning, and a
/// secret is rendered only on explicit request (DESIGN.md section 7: "Only configuration work
/// belongs in a normal config plan").
fn in_scope(r: &ResolvedResource) -> bool {
    matches!(
        r.kind.as_str(),
        "link" | "render" | "merge" | "assemble" | "distribution"
    )
}

fn parts(path: &str) -> Vec<&str> {
    path.split('/').collect()
}

/// One ledger output, with the path relative to HOME.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owned {
    pub resource: String,
    pub adapter: String,
    pub class: Class,
    pub path: String,
    pub fingerprint: Option<String>,
}

pub struct Context<'a> {
    pub binary: Binary,
    pub profile: &'a str,
    pub machine_id: &'a str,
    pub sources: Vec<Source>,
    /// The canonical absolute root the sources were read from. A link points into it.
    pub root: &'a str,
    pub ledger: Vec<Owned>,
    /// HOME-relative claimed paths that overlap the manager's own paths, with the reason. The
    /// caller decides this with the store's rules (text scope and filesystem identity).
    pub reserved: BTreeMap<String, String>,
    /// HOME-relative claimed paths for which that check failed for another reason, with it.
    pub unchecked: BTreeMap<String, String>,
}

/// Ledger outputs of a resource that is no longer selected, at a path no selected resource
/// claims: candidates for removal. A resource that is still selected keeps its outputs even when
/// it claims nothing now, and a path that another resource claims is that resource's business.
fn removals<'a>(resolved: &Resolved, ledger: &'a [Owned]) -> Vec<&'a Owned> {
    let claimed: BTreeSet<String> = resolved
        .resources
        .values()
        .flat_map(|r| r.claims.iter().map(|c| fold(c)))
        .collect();
    ledger
        .iter()
        .filter(|o| {
            !resolved.resources.contains_key(&o.resource) && !claimed.contains(&fold(&o.path))
        })
        .collect()
}

/// The local facts the first observation pass must collect: every claimed path with its
/// parents, every fragment directory with its listing, and every ledger output that is no
/// longer claimed. A sensitive path is never hashed.
pub fn requests(resolved: &Resolved, ledger: &[Owned]) -> Vec<Request> {
    let mut want: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    for r in resolved.resources.values().filter(|r| in_scope(r)) {
        for c in &r.claims {
            want.entry(c.clone()).or_default().0 |= !r.sensitive;
        }
        for d in &r.dir_claims {
            want.entry(d.dir.clone()).or_default().1 = true;
        }
    }
    for o in removals(resolved, ledger) {
        want.entry(o.path.clone()).or_default().0 |= o.class == Class::Normal;
    }
    // Whatever else asked for it, a path that a selected sensitive resource claims is never hashed.
    for r in resolved.resources.values().filter(|r| r.sensitive) {
        for c in &r.claims {
            if let Some(w) = want.get_mut(c) {
                w.0 = false;
            }
        }
    }
    want.into_iter()
        .map(|(path, (hash, list))| Request { path, hash, list })
        .collect()
}

/// The second pass: fragments already in a fragment directory that no resource claims. They are
/// inputs the manager does not produce, so they are hashed. A claimed path is never requested
/// here: whether it is hashed depends on its sensitivity and was decided in the first pass.
pub fn followups(resolved: &Resolved, obs: &Observations) -> Vec<Request> {
    let claimed: BTreeSet<String> = resolved
        .resources
        .values()
        .flat_map(|r| r.claims.iter().map(|c| fold(c)))
        .collect();
    let mut out = BTreeSet::new();
    for r in resolved.resources.values().filter(|r| in_scope(r)) {
        for d in &r.dir_claims {
            let Some(l) = obs.listings.get(&d.dir) else {
                continue;
            };
            for n in l.names.iter().filter(|n| matches_pattern(&d.pattern, n)) {
                let path = format!("{}/{n}", d.dir);
                if !claimed.contains(&fold(&path)) {
                    out.insert(Request {
                        path,
                        hash: true,
                        list: false,
                    });
                }
            }
        }
    }
    out.into_iter().collect()
}

fn matches_pattern(pattern: &str, name: &str) -> bool {
    glob(&fold(pattern), &fold(name))
}

/// A failed check, before it is tied to a resource.
struct Fail {
    code: Code,
    field: &'static str,
    detail: String,
    correction: &'static str,
}

fn fail(code: Code, field: &'static str, detail: String, correction: &'static str) -> Fail {
    Fail {
        code,
        field,
        detail,
        correction,
    }
}

/// The result of walking the parents of one path.
struct Parents {
    /// Index of the first component that does not exist, when one is missing.
    missing_from: Option<usize>,
    /// Preconditions for the existing parents.
    states: Vec<Precondition>,
}

struct Planner<'a> {
    cx: &'a Context<'a>,
    obs: &'a Observations,
    /// Ledger index by folded path.
    ledger: BTreeMap<String, &'a Owned>,
    ops: BTreeMap<String, Operation>,
    blockers: Vec<Blocker>,
    /// Operations made for claims, and for removals. Edges look at these sets, never at the
    /// action, so a blocked or absent removal cannot count as a producer.
    leaf_ids: BTreeSet<String>,
    removal_ids: BTreeSet<String>,
}

pub fn build(resolved: &Resolved, cx: &Context<'_>, obs: &Observations) -> Result<Plan, Refusal> {
    let mut ledger = BTreeMap::new();
    for o in &cx.ledger {
        ledger.entry(fold(&o.path)).or_insert(o);
    }
    let mut p = Planner {
        cx,
        obs,
        ledger,
        ops: BTreeMap::new(),
        blockers: Vec::new(),
        leaf_ids: BTreeSet::new(),
        removal_ids: BTreeSet::new(),
    };
    for r in resolved.resources.values() {
        p.resource(r);
    }
    for o in removals(resolved, &cx.ledger) {
        p.remove(o);
    }
    p.edges(resolved);
    let (mut operations, cycle) = p.finish();
    let mut blockers = p.blockers;
    if let Some(cycle) = cycle {
        let first = operations
            .iter()
            .find(|o| Some(&o.id) == cycle.first())
            .map(|o| (o.resource.clone(), o.path.clone()));
        let (resource, path) = first.unwrap_or_default();
        let source = resolved
            .resources
            .get(&resource)
            .map_or_else(String::new, |r| r.declared_in.clone());
        blockers.push(Blocker {
            code: Code::PlanCycle,
            resource,
            source,
            field: Some("/fragments_dir".into()),
            path,
            detail: format!(
                "operations form a cycle: {}. Each output is a fragment of the next one",
                cycle.join(" -> ")
            ),
            correction: "move one output out of the other's fragment directory, or change a fragment pattern so the output no longer matches".into(),
        });
    }
    let model_digest = model_digest(resolved)?;
    let trees: Vec<&str> = cx.sources.iter().map(|s| s.tree.as_str()).collect();
    for o in &mut operations {
        o.journal_id = journal_id(&model_digest, &trees, &o.id);
    }
    Ok(Plan {
        schema: super::SCHEMA,
        command: "plan".into(),
        scope: "config".into(),
        binary: cx.binary.clone(),
        profile: cx.profile.into(),
        machine_id: cx.machine_id.into(),
        model_digest,
        sources: cx.sources.clone(),
        observations: obs.clone(),
        operations,
        blockers,
    })
}

/// A journal operation id (lowercase letters, digits; at most 64 characters) that is the same
/// for the same model, sources and operation, and does not depend on the plan digest, which
/// covers the id itself.
fn journal_id(model_digest: &str, trees: &[&str], op_id: &str) -> String {
    let h = sha256_hex(format!("{model_digest}\n{}\n{op_id}", trees.join(",")).as_bytes());
    format!("p{}", &h[..32])
}

/// Digest of the resolved model. Values of secret-bearing resources are left out: they are
/// references, and no hash of secret material belongs in a plan (DESIGN.md section 8).
fn model_digest(resolved: &Resolved) -> Result<String, Refusal> {
    // Serializing these types cannot fail. If it ever does, it is reported as the same internal
    // class `state::canonical` uses (E_STATE_INVALID), never as an I/O failure.
    let ser = |e: serde_json::Error| {
        Refusal::new(
            Code::StateInvalid,
            format!("cannot serialize the model: {e}"),
        )
    };
    let mut doc = serde_json::Map::new();
    for (id, r) in &resolved.resources {
        let value = if r.secret_values {
            serde_json::Value::Null
        } else {
            serde_json::to_value(&r.value).map_err(ser)?
        };
        doc.insert(
            id.clone(),
            serde_json::json!({
                "module": r.module, "adapter": r.adapter, "kind": r.kind, "target": r.target,
                "claims": r.claims, "value": value,
                "dirs": r.dir_claims.iter().map(|d| [&d.dir, &d.pattern]).collect::<Vec<_>>(),
            }),
        );
    }
    let all = serde_json::json!({
        "profile": resolved.profile, "modules": resolved.modules, "resources": doc,
    });
    Ok(sha256_hex(&serde_json::to_vec(&all).map_err(ser)?))
}

fn pointer_escape(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

/// The mode a resource declares for its output. Only the permission bits are allowed: a value
/// outside `0..=0o777` (including setuid, setgid and sticky) is refused, not dropped.
fn declared_mode(r: &ResolvedResource) -> Result<Option<u32>, Fail> {
    match r.value.get("mode") {
        None => Ok(None),
        Some(Value::Integer(m)) if (0..=0o777).contains(m) => Ok(u32::try_from(*m).ok()),
        Some(_) => Err(fail(
            Code::ManifestInvalid,
            "/mode",
            "the declared mode is not an integer from 0 to 0o777; setuid, setgid and sticky bits are not allowed".into(),
            "declare permission bits only, for example 0o644",
        )),
    }
}

impl Planner<'_> {
    fn push(&mut self, resource: &str, source: &str, path: Option<&str>, f: Fail) {
        self.blockers.push(Blocker {
            code: f.code,
            resource: resource.into(),
            source: source.into(),
            field: Some(f.field.into()),
            path: path.map(String::from),
            detail: f.detail,
            correction: f.correction.into(),
        });
    }

    fn blocker(&mut self, r: &ResolvedResource, path: Option<&str>, f: Fail) {
        self.push(&r.id, &r.declared_in, path, f);
    }

    fn seen(&self, path: &str) -> Result<&Seen, Fail> {
        self.obs.seen.get(path).ok_or_else(|| {
            fail(
                Code::ObservationUnknown,
                "target",
                format!("{path} was not observed"),
                "run plan again; if it repeats, report the path",
            )
        })
    }

    /// What a claimed path may not be: outside the ASCII names the planner can compare safely
    /// (Unicode normalization is not folded, so two spellings could be one entry), or a path the
    /// manager owns.
    fn claim_checks(&self, path: &str, field: &'static str) -> Result<(), Fail> {
        if let Some(why) = self.cx.reserved.get(path) {
            return Err(fail(
                Code::TargetReserved,
                field,
                format!("{path} overlaps a path the manager owns: {why}"),
                "claim a path outside the manager's state, config, lock, data and cache locations",
            ));
        }
        if !path.is_ascii() {
            return Err(non_ascii(field, path));
        }
        Ok(())
    }

    /// A path the manager's own paths could not be checked against (the check failed for a
    /// reason other than an overlap). It runs after the topology walk, so a link loop or an
    /// unreadable parent is reported with its own, more specific code first.
    fn unchecked(&self, path: &str, field: &'static str) -> Result<(), Fail> {
        match self.cx.unchecked.get(path) {
            None => Ok(()),
            Some(why) => Err(fail(
                Code::ObservationUnknown,
                field,
                format!("{path} could not be checked against the manager's own paths: {why}"),
                "make the path readable and run plan again",
            )),
        }
    }

    /// Walks the parents of `path` (every proper prefix). `write` means entries will be created
    /// inside the deepest existing directory, so it must be writable by the current user.
    fn parents(&self, path: &str, field: &'static str, write: bool) -> Result<Parents, Fail> {
        let comps = parts(path);
        let mut states = Vec::new();
        let mut deepest: Option<(String, u32)> = None;
        for i in 0..comps.len().saturating_sub(1) {
            let p = comps[..=i].join("/");
            let seen = self.seen(&p)?;
            if let Some(actual) = &seen.alias {
                return Err(alias_fail(field, &p, actual));
            }
            match &seen.node {
                Node::Missing {} => {
                    if write
                        && let Some((d, mode)) = &deepest
                        && mode & 0o200 == 0
                    {
                        return Err(unwritable(field, d));
                    }
                    return Ok(Parents {
                        missing_from: Some(i),
                        states,
                    });
                }
                Node::Dir { mode, own } => {
                    if !own {
                        return Err(privilege(field, &p, "is owned by another user"));
                    }
                    states.push(Precondition {
                        subject: format!("path:{p}"),
                        state: node_state(seen, false),
                    });
                    deepest = Some((p, *mode));
                }
                Node::Link { to, target, .. } => return Err(link_fail(field, &p, target, *to)),
                Node::File { .. } | Node::Other { .. } => {
                    return Err(fail(
                        Code::Conflict,
                        field,
                        format!("{p} is a file where a directory is needed"),
                        "move the file aside; the manager never replaces a file with a directory",
                    ));
                }
                Node::Unknown { reason } => return Err(unknown_fail(field, &p, reason)),
            }
        }
        if write
            && let Some((d, mode)) = &deepest
            && mode & 0o200 == 0
        {
            return Err(unwritable(field, d));
        }
        Ok(Parents {
            missing_from: None,
            states,
        })
    }

    fn resource(&mut self, r: &ResolvedResource) {
        if !in_scope(r) {
            self.out_of_scope(r);
            return;
        }
        for claim in &r.claims {
            self.leaf(r, claim);
        }
        for d in &r.dir_claims {
            self.fragment_dir(r, &d.dir);
        }
    }

    fn skeleton(&self, r: &ResolvedResource, id: String, path: Option<String>) -> Operation {
        Operation {
            id,
            journal_id: String::new(),
            resource: r.id.clone(),
            module: r.module.clone(),
            adapter: r.adapter.clone(),
            class: if r.sensitive {
                Class::Sensitive
            } else {
                Class::Normal
            },
            action: Action::None,
            status: Status::Blocked,
            path,
            mode: None,
            link: None,
            privilege: "user".into(),
            effect: String::new(),
            ownership: Ownership::Absent,
            inputs: Vec::new(),
            fragments: Vec::new(),
            after: Vec::new(),
            preconditions: Vec::new(),
            postcondition: "none".into(),
        }
    }

    fn out_of_scope(&mut self, r: &ResolvedResource) {
        let secret = r.kind == "secret";
        let (path, effect) = if secret {
            (
                r.target.clone(),
                "secret output: destination and mode only; rendering needs an explicit secrets scope"
                    .to_string(),
            )
        } else {
            (
                None,
                format!("{} is provisioning, outside a configuration plan", r.kind),
            )
        };
        let id = format!("{}@scope", r.id);
        let mut op = self.skeleton(r, id.clone(), path);
        op.status = Status::NotInScope;
        op.effect = effect;
        match declared_mode(r) {
            Ok(m) => op.mode = m,
            Err(f) => self.blocker(r, op.path.clone().as_deref(), f),
        }
        self.ops.insert(id, op);
    }

    /// The source files that feed one claim, and the absolute link target when the claim is a
    /// link to a source.
    fn inputs(&self, r: &ResolvedResource, claim: &str) -> (Vec<Input>, Option<String>) {
        let wanted = |ptr: &str| match r.kind.as_str() {
            "distribution" => {
                let name = claim.rsplit('/').next().unwrap_or(claim);
                ptr == format!("/entries/{}/source", pointer_escape(name))
            }
            _ => true,
        };
        let inputs: Vec<Input> = r
            .provenance
            .iter()
            .filter(|(ptr, p)| p.source_path.is_some() && wanted(ptr))
            .map(|(_, p)| Input {
                path: p.source_path.clone().unwrap_or_default(),
                content_id: p.content_id.clone(),
            })
            .collect();
        let link = matches!(r.kind.as_str(), "link" | "distribution")
            .then(|| {
                inputs
                    .first()
                    .map(|i| format!("{}/{}", self.cx.root, i.path))
            })
            .flatten();
        (inputs, link)
    }

    fn ledger_entry(&self, path: &str) -> Option<&Owned> {
        self.ledger.get(&fold(path)).copied()
    }

    fn ownership(&self, resource: &str, sensitive: bool, path: &str, seen: &Seen) -> Ownership {
        if matches!(seen.node, Node::Missing {}) {
            return Ownership::Absent;
        }
        let Some(e) = self.ledger_entry(path) else {
            return Ownership::Foreign;
        };
        if e.resource != resource {
            return Ownership::Foreign;
        }
        let live = match &seen.node {
            Node::File { sha256, .. } => sha256.clone(),
            Node::Link { target, .. } => Some(link_fingerprint(target)),
            _ => None,
        };
        match (&e.fingerprint, e.class, live) {
            (Some(want), Class::Normal, Some(have)) if !sensitive => {
                if *want == have {
                    Ownership::Owned
                } else {
                    Ownership::Modified
                }
            }
            _ => Ownership::Unverifiable,
        }
    }

    /// The ledger entry for a path as a precondition string. For a sensitive path the
    /// fingerprint is left out, whoever owns the entry: it can be a hash of credentials.
    fn ledger_state(&self, path: &str, sensitive: bool) -> String {
        self.ledger_entry(path).map_or_else(
            || "none".into(),
            |e| match (&e.fingerprint, sensitive) {
                (Some(f), false) => format!("{}:{f}", e.resource),
                _ => format!("{}:sensitive", e.resource),
            },
        )
    }

    fn leaf(&mut self, r: &ResolvedResource, claim: &str) {
        let field = if r.kind == "distribution" {
            "/entries"
        } else {
            "target"
        };
        let id = format!("{}@{claim}", r.id);
        self.leaf_ids.insert(id.clone());
        let (inputs, link) = self.inputs(r, claim);
        let mut op = self.skeleton(r, id.clone(), Some(claim.into()));
        op.inputs = inputs;
        for i in &op.inputs {
            op.preconditions.push(Precondition {
                subject: format!("source:{}", i.path),
                state: i.content_id.clone().unwrap_or_else(|| "unhashed".into()),
            });
        }
        if let Err(f) = self.leaf_checks(r, claim, field, link.as_deref(), &mut op) {
            op.effect = format!("blocked: {}", f.detail);
            op.status = Status::Blocked;
            self.blocker(r, Some(claim), f);
        }
        self.ops.insert(id, op);
    }

    fn leaf_checks(
        &mut self,
        r: &ResolvedResource,
        claim: &str,
        field: &'static str,
        link: Option<&str>,
        op: &mut Operation,
    ) -> Result<(), Fail> {
        self.claim_checks(claim, field)?;
        op.mode = declared_mode(r)?;
        let parents = self.parents(claim, field, true)?;
        self.unchecked(claim, field)?;
        op.preconditions.extend(parents.states.clone());
        let comps = parts(claim);
        let mut prev: Option<String> = None;
        if let Some(from) = parents.missing_from {
            for i in from..comps.len() - 1 {
                prev = Some(self.mkdir(r, &comps[..=i].join("/"), prev.take()));
            }
            op.after.extend(prev);
        }
        let seen = if parents.missing_from.is_some() {
            Seen {
                node: Node::Missing {},
                alias: None,
            }
        } else {
            self.seen(claim)?.clone()
        };
        // An alias is the same entry under another spelling. Nothing is planned through it,
        // even when the ledger owns the entry.
        if let Some(actual) = &seen.alias {
            return Err(alias_fail(field, claim, actual));
        }
        op.preconditions.push(Precondition {
            subject: format!("path:{claim}"),
            state: node_state(&seen, r.sensitive),
        });
        op.ownership = self.ownership(&r.id, r.sensitive, claim, &seen);
        op.preconditions.push(Precondition {
            subject: format!("ledger:{claim}"),
            state: self.ledger_state(claim, r.sensitive),
        });
        let (action, status, ownership, effect, post) =
            decide(r, claim, &seen, link, op.ownership)?;
        op.action = action;
        op.status = status;
        op.ownership = ownership;
        op.effect = effect;
        op.postcondition = post;
        op.link = link.map(String::from);
        Ok(())
    }

    /// A ledger output that nothing selects any more. It is removed only when the ledger proves
    /// the live output is the one the manager wrote. Anything else is a conflict.
    fn remove(&mut self, o: &Owned) {
        let id = format!("remove@{}@{}", o.resource, o.path);
        self.removal_ids.insert(id.clone());
        let mut op = Operation {
            id: id.clone(),
            journal_id: String::new(),
            resource: o.resource.clone(),
            module: String::new(),
            adapter: o.adapter.clone(),
            class: o.class,
            action: Action::None,
            status: Status::Blocked,
            path: Some(o.path.clone()),
            mode: None,
            link: None,
            privilege: "user".into(),
            effect: String::new(),
            ownership: Ownership::Absent,
            inputs: Vec::new(),
            fragments: Vec::new(),
            after: Vec::new(),
            preconditions: Vec::new(),
            postcondition: "none".into(),
        };
        if let Err(f) = self.remove_checks(o, &mut op) {
            op.effect = format!("blocked: {}", f.detail);
            op.status = Status::Blocked;
            self.push(&o.resource, "state/ledger.json", Some(&o.path), f);
        }
        self.ops.insert(id, op);
    }

    fn remove_checks(&self, o: &Owned, op: &mut Operation) -> Result<(), Fail> {
        let field = "owned_outputs";
        let path = o.path.as_str();
        if !path.is_ascii() {
            return Err(non_ascii(field, path));
        }
        let parents = self.parents(path, field, true)?;
        op.preconditions.extend(parents.states.clone());
        let seen = if parents.missing_from.is_some() {
            Seen {
                node: Node::Missing {},
                alias: None,
            }
        } else {
            self.seen(path)?.clone()
        };
        if let Some(actual) = &seen.alias {
            return Err(alias_fail(field, path, actual));
        }
        op.preconditions.push(Precondition {
            subject: format!("path:{path}"),
            state: node_state(&seen, o.class == Class::Sensitive),
        });
        op.preconditions.push(Precondition {
            subject: format!("ledger:{path}"),
            state: self.ledger_state(path, o.class == Class::Sensitive),
        });
        match &seen.node {
            Node::Missing {} => {
                op.status = Status::Current;
                op.effect = format!("{path} is already absent; no longer selected");
                return Ok(());
            }
            Node::Unknown { reason } => return Err(unknown_fail(field, path, reason)),
            Node::File { own: false, .. }
            | Node::Link { own: false, .. }
            | Node::Other { own: false, .. } => {
                return Err(privilege(field, path, "is owned by another user"));
            }
            Node::Dir { .. } => {
                return Err(fail(
                    Code::Conflict,
                    field,
                    format!("{path} is a directory; the manager never removes a directory"),
                    "remove it yourself if it is no longer wanted",
                ));
            }
            _ => {}
        }
        op.ownership = self.ownership(&o.resource, o.class == Class::Sensitive, path, &seen);
        let why = match op.ownership {
            Ownership::Owned => None,
            Ownership::Modified => Some("was changed after the manager wrote it"),
            Ownership::Unverifiable => Some("has an owner the manager cannot verify"),
            _ => Some("is not owned by this resource according to the ledger"),
        };
        if let Some(why) = why {
            return Err(fail(
                Code::Conflict,
                field,
                format!("{path} is no longer selected but {why}"),
                "review the file and remove it yourself; the manager removes only what it can prove it wrote",
            ));
        }
        op.action = Action::Remove;
        op.status = Status::Planned;
        op.effect = format!("remove {path}; no longer selected");
        op.postcondition = format!("{path} is absent");
        Ok(())
    }

    /// A directory the plan must create. One operation per directory, shared by every claim
    /// below it, chained so a child follows its parent.
    fn mkdir(&mut self, r: &ResolvedResource, path: &str, parent: Option<String>) -> String {
        let id = format!("mkdir@{path}");
        if !self.ops.contains_key(&id) {
            let mut op = self.skeleton(r, id.clone(), Some(path.into()));
            op.class = Class::Normal;
            op.mode = None;
            op.action = Action::CreateDir;
            op.status = Status::Planned;
            op.effect = format!("create directory {path}");
            op.after = parent.into_iter().collect();
            op.preconditions = vec![Precondition {
                subject: format!("path:{path}"),
                state: "missing".into(),
            }];
            op.postcondition = format!("{path} is a real directory");
            self.ops.insert(id.clone(), op);
        }
        id
    }

    /// A fragment directory is read, not written: it must be absent or a real directory.
    fn fragment_dir(&mut self, r: &ResolvedResource, dir: &str) {
        let check = self
            .claim_checks(dir, "/fragments_dir")
            .and_then(|()| self.parents(dir, "/fragments_dir", false))
            .and_then(|p| self.unchecked(dir, "/fragments_dir").map(|()| p))
            .and_then(|p| {
                if p.missing_from.is_some() {
                    return Ok(());
                }
                let seen = self.seen(dir)?;
                if let Some(a) = &seen.alias {
                    return Err(alias_fail("/fragments_dir", dir, a));
                }
                match &seen.node {
                    Node::Missing {} => Ok(()),
                    Node::Dir { own: true, .. } => Ok(()),
                    Node::Dir { .. } => {
                        Err(privilege("/fragments_dir", dir, "is owned by another user"))
                    }
                    Node::Link { target, to, .. } => {
                        Err(link_fail("/fragments_dir", dir, target, *to))
                    }
                    Node::Unknown { reason } => Err(unknown_fail("/fragments_dir", dir, reason)),
                    _ => Err(fail(
                        Code::Conflict,
                        "/fragments_dir",
                        format!("{dir} is not a directory"),
                        "move the file aside or point fragments_dir at a directory",
                    )),
                }
            });
        if let Err(f) = check {
            self.blocker(r, Some(dir), f);
            return;
        }
        if let Some(why) = self.obs.listings.get(dir).and_then(|l| l.incomplete) {
            let (detail, fix) = match why {
                Incomplete::TooMany => (
                    format!("{dir} has too many entries to list"),
                    "remove entries that are not fragments",
                ),
                Incomplete::Failed => (
                    format!("{dir} could not be listed"),
                    "make the directory readable and run plan again",
                ),
                Incomplete::NonUtf8 => (
                    format!("{dir} holds an entry whose name is not UTF-8"),
                    "rename or remove that entry",
                ),
            };
            self.blocker(
                r,
                Some(dir),
                fail(Code::ObservationUnknown, "/fragments_dir", detail, fix),
            );
        }
    }

    /// Why a foreign fragment cannot be used as an input, if it cannot.
    fn fragment_problem(&self, dir: &str, name: &str) -> Option<Fail> {
        let path = format!("{dir}/{name}");
        let field = "/fragments_dir";
        if !name.is_ascii() {
            return Some(fail(
                Code::TargetInvalid,
                field,
                format!(
                    "fragment {path} has non-ASCII characters; Unicode normalization is not supported yet"
                ),
                "rename the fragment with an ASCII name",
            ));
        }
        let fix = "remove or replace it with a regular file you own";
        match self.obs.seen.get(&path).map(|s| (&s.node, &s.alias)) {
            Some((
                Node::File {
                    own: true,
                    sha256: Some(_),
                    ..
                },
                None,
            )) => None,
            Some((Node::File { sha256: None, .. } | Node::Unknown { .. }, _)) | None => Some(fail(
                Code::ObservationUnknown,
                field,
                format!("fragment {path} could not be read or is too large to hash"),
                "make it readable and within 16 MiB, or remove it",
            )),
            Some((Node::File { own: false, .. }, _)) => Some(fail(
                Code::PrivilegeUnsupported,
                field,
                format!("fragment {path} is owned by another user"),
                fix,
            )),
            Some((Node::Link { .. }, _)) => Some(fail(
                Code::Conflict,
                field,
                format!("fragment {path} is a link; its content is not what the plan observed"),
                fix,
            )),
            Some((_, _)) => Some(fail(
                Code::Conflict,
                field,
                format!("fragment {path} is not a regular file"),
                fix,
            )),
        }
    }

    /// Producer to consumer edges for fragment directories, and the fragment inputs.
    fn edges(&mut self, resolved: &Resolved) {
        for r in resolved.resources.values().filter(|r| in_scope(r)) {
            for d in &r.dir_claims {
                self.fragment_edges(r, d);
            }
        }
    }

    /// Operations made for claims that put a file directly in `dir` and match `pattern`:
    /// (operation id, resource, file name). Removals and directory creation never produce.
    fn producers(&self, dir: &str, pattern: &str) -> Vec<(String, String, String)> {
        let dir_f = fold(dir);
        let mut out = Vec::new();
        for id in &self.leaf_ids {
            let Some(o) = self.ops.get(id) else { continue };
            let Some((parent, name)) = o.path.as_deref().and_then(|p| p.rsplit_once('/')) else {
                continue;
            };
            if fold(parent) == dir_f && matches_pattern(pattern, name) {
                out.push((id.clone(), o.resource.clone(), name.to_string()));
            }
        }
        out
    }

    /// Planned removals by folded path: (operation id, resource).
    fn planned_removals(&self) -> BTreeMap<String, (String, String)> {
        self.removal_ids
            .iter()
            .filter_map(|id| self.ops.get(id))
            .filter(|o| o.action == Action::Remove && o.status == Status::Planned)
            .filter_map(|o| Some((fold(o.path.as_deref()?), (o.id.clone(), o.resource.clone()))))
            .collect()
    }

    fn fragment_edges(&mut self, r: &ResolvedResource, d: &DirClaim) {
        let consumers: Vec<String> = r
            .claims
            .iter()
            .map(|c| format!("{}@{c}", r.id))
            .filter(|id| self.ops.contains_key(id))
            .collect();
        let selected = self.producers(&d.dir, &d.pattern);
        let selected_names: BTreeSet<String> = selected.iter().map(|(_, _, n)| fold(n)).collect();
        let mut fragments: Vec<Fragment> = selected
            .iter()
            .map(|(_, res, n)| Fragment {
                name: n.clone(),
                origin: format!("selected:{res}"),
            })
            .collect();
        let removals = self.planned_removals();
        let (mut foreign_state, mut removing, mut problems) = (Vec::new(), Vec::new(), Vec::new());
        if let Some(l) = self.obs.listings.get(&d.dir) {
            for n in l.names.iter().filter(|n| matches_pattern(&d.pattern, n)) {
                let path = format!("{}/{n}", d.dir);
                if selected_names.contains(&fold(n)) {
                    continue;
                }
                // A fragment that a planned removal deletes is read first, then removed.
                if let Some((rid, res)) = removals.get(&fold(&path)) {
                    fragments.push(Fragment {
                        name: n.clone(),
                        origin: format!("removing:{res}"),
                    });
                    removing.push((n.clone(), rid.clone()));
                    continue;
                }
                fragments.push(Fragment {
                    name: n.clone(),
                    origin: "foreign".into(),
                });
                let st = self
                    .obs
                    .seen
                    .get(&path)
                    .map_or_else(|| "unobserved".into(), |s| node_state(s, false));
                foreign_state.push(format!("{n}={st}"));
                if let Some(f) = self.fragment_problem(&d.dir, n) {
                    problems.push((path, f));
                }
            }
        }
        fragments.sort_by(|a, b| (&a.name, &a.origin).cmp(&(&b.name, &b.origin)));
        let mut names: Vec<String> = selected.iter().map(|(_, _, n)| n.clone()).collect();
        names.sort();
        let state = format!(
            "selected=[{}] foreign=[{}] removing=[{}]",
            names.join(","),
            foreign_state.join(","),
            removing
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(",")
        );
        let blocked = !problems.is_empty();
        for (path, f) in problems {
            self.blocker(r, Some(&path), f);
        }
        for cid in &consumers {
            if let Some(op) = self.ops.get_mut(cid) {
                for (pid, _, _) in &selected {
                    if !op.after.contains(pid) {
                        op.after.push(pid.clone());
                    }
                }
                op.after.sort();
                op.fragments = fragments.clone();
                op.preconditions.push(Precondition {
                    subject: format!("fragments:{}", d.dir),
                    state: state.clone(),
                });
                if blocked {
                    op.status = Status::Blocked;
                    op.effect = format!("blocked: a fragment in {} cannot be used", d.dir);
                }
            }
        }
        // The removal waits for every consumer that reads the file.
        for (_, rid) in removing {
            if let Some(rm) = self.ops.get_mut(&rid) {
                for cid in &consumers {
                    if !rm.after.contains(cid) {
                        rm.after.push(cid.clone());
                    }
                }
                rm.after.sort();
            }
        }
    }

    /// Orders the operations. On a cycle the members are marked blocked and the cycle returned.
    fn finish(&mut self) -> (Vec<Operation>, Option<Vec<String>>) {
        let nodes: Vec<(String, Vec<String>)> = self
            .ops
            .values()
            .map(|o| (o.id.clone(), o.after.clone()))
            .collect();
        match graph::order(&nodes) {
            Ok(order) => (
                order
                    .into_iter()
                    .filter_map(|id| self.ops.remove(&id))
                    .collect(),
                None,
            ),
            Err(cycle) => {
                for id in &cycle {
                    if let Some(o) = self.ops.get_mut(id) {
                        o.status = Status::Blocked;
                    }
                }
                (
                    std::mem::take(&mut self.ops).into_values().collect(),
                    Some(cycle),
                )
            }
        }
    }
}

type Decision = (Action, Status, Ownership, String, String);

fn decide(
    r: &ResolvedResource,
    path: &str,
    seen: &Seen,
    link: Option<&str>,
    ownership: Ownership,
) -> Result<Decision, Fail> {
    let shown = if r.sensitive {
        format!("{path} (sensitive: destination and mode only)")
    } else {
        path.to_string()
    };
    let conflict = |what: &str| {
        fail(
            Code::Conflict,
            "target",
            format!("{path} {what}"),
            "review the file, then move it aside or adopt it; the manager does not overwrite content it does not own",
        )
    };
    if let Node::Unknown { reason } = &seen.node {
        return Err(unknown_fail("target", path, reason));
    }
    if matches!(seen.node, Node::Dir { .. }) {
        return Err(fail(
            Code::Conflict,
            "target",
            format!(
                "{path} is a directory where a {} is needed",
                if link.is_some() { "link" } else { "file" }
            ),
            "move the directory aside; the manager never replaces a directory",
        ));
    }
    if let Node::File { own: false, .. }
    | Node::Link { own: false, .. }
    | Node::Other { own: false, .. } = &seen.node
    {
        return Err(privilege("target", path, "is owned by another user"));
    }
    match link {
        Some(want) => {
            let post = format!("readlink {path} equals {want}");
            match (&seen.node, ownership) {
                (Node::Missing {}, _) => Ok((
                    Action::CreateLink,
                    Status::Planned,
                    Ownership::Absent,
                    format!("link {shown} -> {want}"),
                    post,
                )),
                (Node::Link { target, .. }, o) if target == want => Ok((
                    Action::None,
                    Status::Current,
                    if o == Ownership::Owned {
                        Ownership::Owned
                    } else {
                        Ownership::UnrecordedCurrent
                    },
                    format!("link {shown} -> {want} already holds"),
                    post,
                )),
                (_, Ownership::Owned) => Ok((
                    Action::ReplaceLink,
                    Status::Planned,
                    Ownership::Owned,
                    format!("replace {shown} with a link -> {want}"),
                    post,
                )),
                (_, Ownership::Modified) => Err(conflict("was changed after the manager wrote it")),
                (_, Ownership::Unverifiable) => {
                    Err(conflict("has an owner the manager cannot verify"))
                }
                _ => Err(conflict(
                    "exists and no ledger entry says this resource owns it",
                )),
            }
        }
        None => {
            let post = format!("{path} holds the rendered content of {}", r.adapter);
            match (&seen.node, ownership) {
                (Node::Missing {}, _) => Ok((
                    Action::CreateFile,
                    Status::Planned,
                    Ownership::Absent,
                    format!("write {shown} with {}", r.adapter),
                    post,
                )),
                (_, Ownership::Owned) => Ok((
                    Action::UpdateFile,
                    Status::Planned,
                    Ownership::Owned,
                    format!("update {shown} with {}", r.adapter),
                    post,
                )),
                (_, Ownership::Modified) => Err(conflict("was changed after the manager wrote it")),
                (_, Ownership::Unverifiable) => Err(conflict(
                    "exists and its ownership cannot be verified: no comparable fingerprint is recorded for it",
                )),
                _ => Err(conflict(
                    "exists and no ledger entry says this resource owns it",
                )),
            }
        }
    }
}

fn non_ascii(field: &'static str, path: &str) -> Fail {
    fail(
        Code::TargetInvalid,
        field,
        format!(
            "{path} contains non-ASCII characters; Unicode normalization is not supported yet, so two spellings could be one entry"
        ),
        "use an ASCII name for the target",
    )
}

fn alias_fail(field: &'static str, path: &str, actual: &str) -> Fail {
    fail(
        Code::TargetCollision,
        field,
        format!(
            "{path} differs from the existing entry `{actual}` only by case; the filesystem treats them as one"
        ),
        "spell the claim as the entry on disk, or move that entry aside",
    )
}

fn link_fail(field: &'static str, path: &str, target: &str, to: LinkTo) -> Fail {
    let why = match to {
        LinkTo::Dangling => "is a dangling link",
        LinkTo::Loop => "is a recursive link",
        LinkTo::Dir => "is a link to a directory, so a write would land outside the claim",
        LinkTo::File | LinkTo::Other => "is a link where a directory is needed",
    };
    fail(
        Code::ParentUnsafe,
        field,
        format!("parent {path} {why} (-> {target})"),
        "replace the link with a real directory yourself; the manager never writes through a link",
    )
}

fn privilege(field: &'static str, path: &str, why: &str) -> Fail {
    fail(
        Code::PrivilegeUnsupported,
        field,
        format!("{path} {why}; a HOME adapter never escalates"),
        "fix the owner yourself, or declare the output under a directory you own",
    )
}

fn unwritable(field: &'static str, dir: &str) -> Fail {
    privilege(field, dir, "is not writable by the current user")
}

fn unknown_fail(field: &'static str, path: &str, reason: &str) -> Fail {
    fail(
        Code::ObservationUnknown,
        field,
        format!("{path} could not be observed: {reason}"),
        "make the path readable and run plan again",
    )
}
