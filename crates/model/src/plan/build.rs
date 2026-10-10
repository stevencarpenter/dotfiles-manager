//! Plan construction: claims, parent topology, ownership, producer and consumer edges.

use super::graph;
use super::{
    Action, Binary, Blocker, Fragment, Input, LinkTo, Node, Observations, Operation, Ownership,
    Plan, Precondition, Request, Seen, Source, Status,
};
use crate::digest::sha256_hex;
use crate::refusal::Code;
use crate::resolve::{Resolved, ResolvedResource, glob};
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

fn fold(s: &str) -> String {
    s.to_lowercase()
}

fn parts(path: &str) -> Vec<&str> {
    path.split('/').collect()
}

/// One ledger output, with the path relative to HOME.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owned {
    pub resource: String,
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
}

/// The local facts the first observation pass must collect: every claimed path with its
/// parents, and every fragment directory with its listing.
pub fn requests(resolved: &Resolved) -> Vec<Request> {
    let mut want: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    for r in resolved.resources.values().filter(|r| in_scope(r)) {
        for c in &r.claims {
            want.entry(c.clone()).or_default().0 |= !r.sensitive;
        }
        for d in &r.dir_claims {
            want.entry(d.dir.clone()).or_default().1 = true;
        }
    }
    want.into_iter()
        .map(|(path, (hash, list))| Request { path, hash, list })
        .collect()
}

/// The second pass: fragments already in a fragment directory, which are inputs the manager
/// does not produce.
pub fn followups(resolved: &Resolved, obs: &Observations) -> Vec<Request> {
    let mut out = BTreeSet::new();
    for r in resolved.resources.values().filter(|r| in_scope(r)) {
        for d in &r.dir_claims {
            let Some(l) = obs.listings.get(&d.dir) else {
                continue;
            };
            for n in l.names.iter().filter(|n| matches_pattern(&d.pattern, n)) {
                out.insert(Request {
                    path: format!("{}/{n}", d.dir),
                    hash: true,
                    list: false,
                });
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
    ops: BTreeMap<String, Operation>,
    blockers: Vec<Blocker>,
}

pub fn build(resolved: &Resolved, cx: &Context<'_>, obs: &Observations) -> Plan {
    let mut p = Planner {
        cx,
        obs,
        ops: BTreeMap::new(),
        blockers: Vec::new(),
    };
    for r in resolved.resources.values() {
        p.resource(r);
    }
    p.edges(resolved);
    let (operations, cycle) = p.finish();
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
    Plan {
        schema: super::SCHEMA,
        command: "plan".into(),
        scope: "config".into(),
        binary: cx.binary.clone(),
        profile: cx.profile.into(),
        machine_id: cx.machine_id.into(),
        model_digest: model_digest(resolved),
        sources: cx.sources.clone(),
        observations: obs.clone(),
        operations,
        blockers,
    }
}

/// Digest of the resolved model. Values of secret-bearing resources are left out: they are
/// references, and no hash of secret material belongs in a plan (DESIGN.md section 8).
fn model_digest(resolved: &Resolved) -> String {
    let mut doc = serde_json::Map::new();
    for (id, r) in &resolved.resources {
        let value = if r.secret_values {
            serde_json::Value::Null
        } else {
            serde_json::to_value(&r.value).unwrap_or(serde_json::Value::Null)
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
    sha256_hex(&serde_json::to_vec(&all).expect("a json value serializes"))
}

fn node_state(seen: &Seen, sensitive: bool) -> String {
    let base = match &seen.node {
        Node::Missing => "missing".to_string(),
        Node::Dir { mode, .. } => format!("dir:{mode:o}"),
        Node::File {
            mode, size, sha256, ..
        } => match (sha256, sensitive) {
            (Some(h), false) => format!("file:{mode:o}:sha256:{h}"),
            (None, false) => format!("file:{mode:o}:size:{size}"),
            (_, true) => format!("file:{mode:o}"),
        },
        Node::Link { target, to, .. } => format!("link:{target}:{to:?}").to_lowercase(),
        Node::Other { mode, .. } => format!("other:{mode:o}"),
        Node::Unknown { reason } => format!("unknown:{reason}"),
    };
    match &seen.alias {
        Some(a) => format!("{base} alias={a}"),
        None => base,
    }
}

fn pointer_escape(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

impl Planner<'_> {
    fn blocker(&mut self, r: &ResolvedResource, path: Option<&str>, f: Fail) {
        self.blockers.push(Blocker {
            code: f.code,
            resource: r.id.clone(),
            source: r.declared_in.clone(),
            field: Some(f.field.into()),
            path: path.map(String::from),
            detail: f.detail,
            correction: f.correction.into(),
        });
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
                Node::Missing => {
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

    fn out_of_scope(&mut self, r: &ResolvedResource) {
        let secret = r.kind == "secret";
        let (path, mode, effect) = if secret {
            let mode = match r.value.get("mode") {
                Some(Value::Integer(m)) => u32::try_from(*m).ok(),
                _ => None,
            };
            (
                r.target.clone(),
                mode,
                "secret output: destination and mode only; rendering needs an explicit secrets scope"
                    .to_string(),
            )
        } else {
            (
                None,
                None,
                format!("{} is provisioning, outside a configuration plan", r.kind),
            )
        };
        let id = format!("{}@scope", r.id);
        self.ops.insert(
            id.clone(),
            Operation {
                id,
                resource: r.id.clone(),
                module: r.module.clone(),
                adapter: r.adapter.clone(),
                class: if r.sensitive {
                    Class::Sensitive
                } else {
                    Class::Normal
                },
                action: Action::None,
                status: Status::NotInScope,
                path,
                mode,
                privilege: "user".into(),
                effect,
                ownership: Ownership::Absent,
                inputs: Vec::new(),
                fragments: Vec::new(),
                after: Vec::new(),
                preconditions: Vec::new(),
                postcondition: "none".into(),
            },
        );
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
        let f = fold(path);
        self.cx.ledger.iter().find(|o| fold(&o.path) == f)
    }

    fn ownership(&self, r: &ResolvedResource, path: &str, seen: &Seen) -> Ownership {
        if matches!(seen.node, Node::Missing) {
            return Ownership::Absent;
        }
        let Some(e) = self.ledger_entry(path) else {
            return Ownership::Foreign;
        };
        if e.resource != r.id {
            return Ownership::Foreign;
        }
        let live = match &seen.node {
            Node::File { sha256, .. } => sha256.clone(),
            Node::Link { target, .. } => Some(sha256_hex(target.as_bytes())),
            _ => None,
        };
        match (&e.fingerprint, e.class, live) {
            (Some(want), Class::Normal, Some(have)) if !r.sensitive => {
                if *want == have {
                    Ownership::Owned
                } else {
                    Ownership::Modified
                }
            }
            _ => Ownership::Unverifiable,
        }
    }

    fn leaf(&mut self, r: &ResolvedResource, claim: &str) {
        let field = if r.kind == "distribution" {
            "/entries"
        } else {
            "target"
        };
        let id = format!("{}@{claim}", r.id);
        let (inputs, link) = self.inputs(r, claim);
        let class = if r.sensitive {
            Class::Sensitive
        } else {
            Class::Normal
        };
        let mut op = Operation {
            id: id.clone(),
            resource: r.id.clone(),
            module: r.module.clone(),
            adapter: r.adapter.clone(),
            class,
            action: Action::None,
            status: Status::Blocked,
            path: Some(claim.into()),
            mode: None,
            privilege: "user".into(),
            effect: String::new(),
            ownership: Ownership::Absent,
            inputs,
            fragments: Vec::new(),
            after: Vec::new(),
            preconditions: Vec::new(),
            postcondition: String::new(),
        };
        for i in &op.inputs {
            op.preconditions.push(Precondition {
                subject: format!("source:{}", i.path),
                state: i.content_id.clone().unwrap_or_else(|| "unhashed".into()),
            });
        }
        let parents = match self.parents(claim, field, true) {
            Ok(p) => p,
            Err(f) => {
                op.effect = format!("blocked: {}", f.detail);
                self.blocker(r, Some(claim), f);
                self.ops.insert(id, op);
                return;
            }
        };
        op.preconditions.extend(parents.states.clone());
        let comps = parts(claim);
        let mut prev: Option<String> = None;
        if let Some(from) = parents.missing_from {
            for i in from..comps.len() - 1 {
                prev = Some(self.mkdir(r, &comps[..=i].join("/"), prev.take()));
            }
            op.after.extend(prev);
        }
        let missing = Seen {
            node: Node::Missing,
            alias: None,
        };
        let seen = if parents.missing_from.is_some() {
            Ok(&missing)
        } else {
            self.seen(claim)
        };
        let seen = match seen {
            Ok(s) => s.clone(),
            Err(f) => {
                op.effect = format!("blocked: {}", f.detail);
                self.blocker(r, Some(claim), f);
                self.ops.insert(id, op);
                return;
            }
        };
        op.preconditions.push(Precondition {
            subject: format!("path:{claim}"),
            state: node_state(&seen, r.sensitive),
        });
        op.ownership = self.ownership(r, claim, &seen);
        op.preconditions.push(Precondition {
            subject: format!("ledger:{claim}"),
            state: self.ledger_entry(claim).map_or_else(
                || "none".into(),
                |e| {
                    format!(
                        "{}:{}",
                        e.resource,
                        e.fingerprint.clone().unwrap_or_else(|| "sensitive".into())
                    )
                },
            ),
        });
        let decision = decide(r, claim, &seen, link.as_deref(), op.ownership);
        match decision {
            Ok((action, status, ownership, effect, post)) => {
                op.action = action;
                op.status = status;
                op.ownership = ownership;
                op.effect = effect;
                op.postcondition = post;
            }
            Err(f) => {
                op.effect = format!("blocked: {}", f.detail);
                op.postcondition = "none".into();
                self.blocker(r, Some(claim), f);
            }
        }
        self.ops.insert(id, op);
    }

    /// A directory the plan must create. One operation per directory, shared by every claim
    /// below it, chained so a child follows its parent.
    fn mkdir(&mut self, r: &ResolvedResource, path: &str, parent: Option<String>) -> String {
        let id = format!("mkdir@{path}");
        self.ops.entry(id.clone()).or_insert_with(|| Operation {
            id: id.clone(),
            resource: r.id.clone(),
            module: r.module.clone(),
            adapter: r.adapter.clone(),
            class: Class::Normal,
            action: Action::CreateDir,
            status: Status::Planned,
            path: Some(path.into()),
            mode: None,
            privilege: "user".into(),
            effect: format!("create directory {path}"),
            ownership: Ownership::Absent,
            inputs: Vec::new(),
            fragments: Vec::new(),
            after: parent.into_iter().collect(),
            preconditions: vec![Precondition {
                subject: format!("path:{path}"),
                state: "missing".into(),
            }],
            postcondition: format!("{path} is a real directory"),
        });
        id
    }

    /// A fragment directory is read, not written: it must be absent or a real directory.
    fn fragment_dir(&mut self, r: &ResolvedResource, dir: &str) {
        let check = self.parents(dir, "/fragments_dir", false).and_then(|p| {
            if p.missing_from.is_some() {
                return Ok(());
            }
            let seen = self.seen(dir)?;
            if let Some(a) = &seen.alias {
                return Err(alias_fail("/fragments_dir", dir, a));
            }
            match &seen.node {
                Node::Missing => Ok(()),
                Node::Dir { own: true, .. } => Ok(()),
                Node::Dir { .. } => {
                    Err(privilege("/fragments_dir", dir, "is owned by another user"))
                }
                Node::Link { target, to, .. } => Err(link_fail("/fragments_dir", dir, target, *to)),
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
        }
        if self.obs.listings.get(dir).is_some_and(|l| l.truncated) {
            self.blocker(
                r,
                Some(dir),
                fail(
                    Code::ObservationUnknown,
                    "/fragments_dir",
                    format!("{dir} has too many entries to list"),
                    "remove entries that are not fragments",
                ),
            );
        }
    }

    /// Producer to consumer edges for fragment directories, and the fragment inputs.
    fn edges(&mut self, resolved: &Resolved) {
        for r in resolved.resources.values().filter(|r| in_scope(r)) {
            for d in &r.dir_claims {
                let consumers: Vec<String> = r
                    .claims
                    .iter()
                    .map(|c| format!("{}@{c}", r.id))
                    .filter(|id| self.ops.contains_key(id))
                    .collect();
                let dir_f = fold(&d.dir);
                let mut selected: Vec<(String, String, String)> = Vec::new();
                for (id, o) in &self.ops {
                    let Some(p) = &o.path else { continue };
                    if o.action == Action::CreateDir || o.status == Status::NotInScope {
                        continue;
                    }
                    let Some((parent, name)) = p.rsplit_once('/') else {
                        continue;
                    };
                    if fold(parent) == dir_f && matches_pattern(&d.pattern, name) {
                        selected.push((id.clone(), o.resource.clone(), name.to_string()));
                    }
                }
                let selected_names: BTreeSet<String> =
                    selected.iter().map(|(_, _, n)| fold(n)).collect();
                let mut fragments: Vec<Fragment> = selected
                    .iter()
                    .map(|(_, res, n)| Fragment {
                        name: n.clone(),
                        origin: format!("selected:{res}"),
                    })
                    .collect();
                let mut foreign_state = Vec::new();
                if let Some(l) = self.obs.listings.get(&d.dir) {
                    for n in l.names.iter().filter(|n| matches_pattern(&d.pattern, n)) {
                        if selected_names.contains(&fold(n)) {
                            continue;
                        }
                        fragments.push(Fragment {
                            name: n.clone(),
                            origin: "foreign".into(),
                        });
                        let st = self
                            .obs
                            .seen
                            .get(&format!("{}/{n}", d.dir))
                            .map_or_else(|| "unobserved".into(), |s| node_state(s, false));
                        foreign_state.push(format!("{n}={st}"));
                    }
                }
                fragments.sort_by(|a, b| (&a.name, &a.origin).cmp(&(&b.name, &b.origin)));
                let mut names: Vec<String> = selected.iter().map(|(_, _, n)| n.clone()).collect();
                names.sort();
                for cid in consumers {
                    if let Some(op) = self.ops.get_mut(&cid) {
                        for (pid, _, _) in &selected {
                            if !op.after.contains(pid) {
                                op.after.push(pid.clone());
                            }
                        }
                        op.after.sort();
                        op.fragments = fragments.clone();
                        op.preconditions.push(Precondition {
                            subject: format!("fragments:{}", d.dir),
                            state: format!(
                                "selected=[{}] foreign=[{}]",
                                names.join(","),
                                foreign_state.join(",")
                            ),
                        });
                    }
                }
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

#[allow(clippy::type_complexity)]
fn decide(
    r: &ResolvedResource,
    path: &str,
    seen: &Seen,
    link: Option<&str>,
    ownership: Ownership,
) -> Result<(Action, Status, Ownership, String, String), Fail> {
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
                (Node::Missing, _) => Ok((
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
                (Node::Missing, _) => Ok((
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
