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

/// Which resources one plan acts on. A configuration plan covers the file kinds. A secret is
/// rendered only by `secrets`, and a package installed only by `packages` (DESIGN.md section 7:
/// "Only configuration work belongs in a normal config plan"). Everything else in a plan is
/// `not_in_scope`, so the counts still add up to the number of participants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Config,
    Secrets,
    Packages,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Config => "config",
            Scope::Secrets => "secrets",
            Scope::Packages => "packages",
        }
    }

    pub fn covers(self, r: &ResolvedResource) -> bool {
        match self {
            Scope::Config => matches!(
                r.kind.as_str(),
                "link" | "render" | "merge" | "assemble" | "distribution"
            ),
            Scope::Secrets => r.kind == "secret",
            Scope::Packages => r.kind == "package",
        }
    }

    /// Why a resource of another scope is listed and not acted on.
    fn excuse(self, r: &ResolvedResource) -> String {
        let what = match r.kind.as_str() {
            "secret" => {
                "secret output: destination and mode only; rendering needs the secrets command"
                    .to_string()
            }
            "package" | "application" => format!("{} is provisioning", r.kind),
            k => format!("{k} is configuration"),
        };
        if r.kind == "secret" && self == Scope::Config {
            return what;
        }
        format!("{what}, outside a {} plan", self.as_str())
    }
}

/// The one secret adapter (docs/secrets.md). The ledger records an adapter, not a kind, so the
/// planner names it to tell a deselected secret output (kept) from any other sensitive output.
pub const SECRET_ADAPTER: &str = "op-template/v1";

/// The kinds a configuration plan covers ([`Scope::Config`]).
pub fn in_scope(r: &ResolvedResource) -> bool {
    Scope::Config.covers(r)
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
    /// `Some((entries, appended))` for a co-owned file (`claude-settings/v1`): how many hook
    /// entries and how many array items the manager added.
    pub coowned_entries: Option<(usize, usize)>,
}

/// What the caller learned from the live file of a co-owned merge resource. The planner is pure
/// and never reads a sensitive file, so the caller reads it and reports only this verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeVerdict {
    /// The merge changes the file, or the ledger needs an entry. The file is absent, or it can
    /// be merged.
    Write,
    /// The merged result equals the live file and the ledger already records it.
    Current,
    /// The file cannot be merged: malformed, open to group or others, or a managed field was
    /// changed outside the manager. The detail carries no live content.
    Blocked {
        code: Code,
        detail: String,
        correction: String,
    },
}

/// What the caller learned from the destination of a secret resource (docs/secrets.md). The
/// planner never reads a secret output; the caller compares the live file with the salted hash
/// the ledger records and reports only this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecretVerdict {
    /// The destination is absent, or the ledger proves the manager wrote what is there.
    Owned,
    /// The destination exists and the ledger does not prove the manager wrote it (no entry, or
    /// the content changed). Overwriting it needs `--allow-adopt`.
    Unowned,
    /// The destination cannot be rendered into: not a regular file of the user, open to group or
    /// others, or the ledger hash cannot be checked. The detail carries no content.
    Blocked {
        code: Code,
        detail: String,
        correction: String,
    },
}

/// What the provider's listing said about a package (docs/packages.md). The planner is pure
/// and starts no process; the caller probes and reports this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageState {
    /// The declared provider lists it, with the version it reports.
    Installed(String),
    /// The declared provider does not list it and no other owner was seen. The names are the
    /// dependencies the install would add with it (empty when there are none, or when the
    /// provider has no dependency listing). They are part of the plan, so the reviewer approves
    /// them too.
    Missing(Vec<String>),
    /// The declared provider does not list it, and installing it would upgrade these installed
    /// dependencies. A package plan upgrades nothing, so this blocks it.
    Upgrades(Vec<String>),
    /// The declared provider does not list it, and these (the package or a missing dependency)
    /// have no prebuilt binary for this machine. A source build adds build-time dependencies the
    /// listing does not name, so the plan could not show them; this blocks it.
    SourceBuild(Vec<String>),
    /// The declared provider does not list it, but an executable of that name exists elsewhere:
    /// a declared owner is not permission to replace another owner (DESIGN.md section 8).
    OtherOwner(String),
    /// The provider cannot install on this machine (for example it needs privilege).
    Unsupported(String),
    /// The probe could not decide. Unknown support fails closed.
    Unknown(String),
    /// The resource value is not a valid package declaration (a bad name). Not a probe result.
    Refused(String),
}

impl PackageState {
    /// Whether a probe at apply time still matches the planned precondition `want`: the same
    /// state, or `missing` with a subset of the planned dependencies. An earlier install in the
    /// same run can add a dependency two packages share; that is not a stale plan.
    pub fn still_planned(&self, want: &str) -> bool {
        if self.state() == want {
            return true;
        }
        let PackageState::Missing(now) = self else {
            return false;
        };
        let planned: BTreeSet<&str> = match want.strip_prefix("missing:with=") {
            Some(list) => list.split(',').collect(),
            None if want == "missing" => BTreeSet::new(),
            None => return false,
        };
        now.iter().all(|d| planned.contains(d.as_str()))
    }

    /// The precondition string apply compares after it probes again.
    pub fn state(&self) -> String {
        match self {
            PackageState::Installed(v) => format!("installed:{v}"),
            PackageState::Missing(d) if d.is_empty() => "missing".into(),
            PackageState::Missing(d) => format!("missing:with={}", d.join(",")),
            PackageState::Upgrades(d) => format!("upgrades:{}", d.join(",")),
            PackageState::SourceBuild(d) => format!("source-build:{}", d.join(",")),
            PackageState::OtherOwner(w) => format!("other-owner:{w}"),
            PackageState::Unsupported(w) => format!("unsupported:{w}"),
            PackageState::Unknown(w) => format!("unknown:{w}"),
            PackageState::Refused(w) => format!("refused:{w}"),
        }
    }
}

/// The plan-time facts of one merge resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeFact {
    /// SHA-256 of the canonical JSON of the declared managed values.
    pub managed_sha256: String,
    /// Names of the top-level settings the merge sets or seeds. Names only.
    pub sets: Vec<String>,
    /// How many handler entries the merge adds.
    pub handlers: usize,
    /// The fragments the merge read, by name in byte order, each with a content token. The token
    /// is a salted hash from the CLI (empty while `state/salt` does not exist yet). The planner
    /// compares the names with its own fragment list and puts the tokens in the `fragments:`
    /// precondition, so a changed fragment makes a saved plan stale. A token never reaches text
    /// output (docs/adapters.md, "Sensitive policy").
    pub fragments: Vec<(String, String)>,
    pub verdict: MergeVerdict,
}

/// The ledger entry for a path as a precondition string. For a sensitive path the fingerprint
/// is left out, whoever owns the entry: it can be a hash of credentials. Apply builds the same
/// string from a fresh ledger read, so both sides must use this function.
pub fn ledger_state(entry: Option<&Owned>, sensitive: bool) -> String {
    entry.map_or_else(
        || "none".into(),
        |e| match (&e.fingerprint, sensitive) {
            (Some(f), false) => format!("{}:{f}", e.resource),
            _ => format!("{}:sensitive", e.resource),
        },
    )
}

pub struct Context<'a> {
    /// Which resources this plan acts on. The others are listed as `not_in_scope`.
    pub scope: Scope,
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
    /// Facts about the live file of each selected merge resource, by resource id.
    pub merge: BTreeMap<String, MergeFact>,
    /// The verdict on the destination of each selected secret resource, by resource id
    /// (`secrets` scope only).
    pub secret: BTreeMap<String, SecretVerdict>,
    /// The probed state of each selected package resource, by resource id (`packages` scope
    /// only).
    pub package: BTreeMap<String, PackageState>,
}

/// Ledger outputs of a resource that is no longer selected, at a path no selected resource
/// claims: candidates for removal. A resource that is still selected keeps its outputs even when
/// it claims nothing now, and a path that another resource claims is that resource's business.
fn removals<'a>(resolved: &Resolved, ledger: &'a [Owned], scope: Scope) -> Vec<&'a Owned> {
    // Removal is configuration work: a secrets or packages plan removes nothing.
    if scope != Scope::Config {
        return Vec::new();
    }
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
pub fn requests(resolved: &Resolved, ledger: &[Owned], scope: Scope) -> Vec<Request> {
    let mut want: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    for r in resolved.resources.values().filter(|r| scope.covers(r)) {
        for c in &r.claims {
            want.entry(c.clone()).or_default().0 |= !r.sensitive;
        }
        for d in &r.dir_claims {
            want.entry(d.dir.clone()).or_default().1 = true;
        }
    }
    for o in removals(resolved, ledger, scope) {
        want.entry(o.path.clone()).or_default().0 |= o.class == Class::Normal;
    }
    // Whatever else asked for it, a path that a selected sensitive resource claims, or that the
    // ledger records as sensitive (whoever claims it now), is never hashed.
    let sensitive_ledger = sensitive_ledger_paths(ledger);
    for (path, w) in &mut want {
        if sensitive_ledger.contains(&fold(path)) {
            w.0 = false;
        }
    }
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

/// Folded paths whose content nothing may read or hash: ledger entries of the sensitive class
/// (whoever claims the path now), ledger entries owned by a selected sensitive resource, and the
/// claims of selected sensitive resources. `resolved` is
/// `None` when the profile could not be resolved; the ledger still protects its own entries.
pub fn sensitive_paths(resolved: Option<&Resolved>, ledger: &[Owned]) -> BTreeSet<String> {
    let mut set = sensitive_ledger_paths(ledger);
    for r in resolved
        .iter()
        .flat_map(|r| r.resources.values())
        .filter(|r| r.sensitive)
    {
        set.extend(r.claims.iter().map(|c| fold(c)));
    }
    if let Some(res) = resolved {
        set.extend(
            ledger
                .iter()
                .filter(|o| res.resources.get(&o.resource).is_some_and(|r| r.sensitive))
                .map(|o| fold(&o.path)),
        );
    }
    set
}

/// Folded paths the ledger records with the sensitive class.
fn sensitive_ledger_paths(ledger: &[Owned]) -> BTreeSet<String> {
    ledger
        .iter()
        .filter(|o| o.class == Class::Sensitive)
        .map(|o| fold(&o.path))
        .collect()
}

/// The second pass: fragments already in a fragment directory that no resource claims. They are
/// inputs the manager does not produce, so they are hashed. A claimed path is never requested
/// here: whether it is hashed depends on its sensitivity and was decided in the first pass.
pub fn followups(
    resolved: &Resolved,
    obs: &Observations,
    ledger: &[Owned],
    scope: Scope,
) -> Vec<Request> {
    let sensitive = sensitive_ledger_paths(ledger);
    let claimed: BTreeSet<String> = resolved
        .resources
        .values()
        .flat_map(|r| r.claims.iter().map(|c| fold(c)))
        .collect();
    let mut out = BTreeSet::new();
    for r in resolved.resources.values().filter(|r| scope.covers(r)) {
        for d in &r.dir_claims {
            let Some(l) = obs.listings.get(&d.dir) else {
                continue;
            };
            for n in l.names.iter().filter(|n| matches_pattern(&d.pattern, n)) {
                let path = format!("{}/{n}", d.dir);
                let folded = fold(&path);
                if !claimed.contains(&folded) && !sensitive.contains(&folded) {
                    // A fragment of a sensitive merge can hold credentials: it is never hashed.
                    out.insert(Request {
                        path,
                        hash: !r.sensitive,
                        list: false,
                    });
                }
            }
        }
    }
    out.into_iter().collect()
}

pub fn matches_pattern(pattern: &str, name: &str) -> bool {
    glob(&fold(pattern), &fold(name))
}

/// A failed check, before it is tied to a resource.
struct Fail {
    code: Code,
    field: &'static str,
    detail: String,
    correction: std::borrow::Cow<'static, str>,
}

fn fail(
    code: Code,
    field: &'static str,
    detail: String,
    correction: impl Into<std::borrow::Cow<'static, str>>,
) -> Fail {
    Fail {
        code,
        field,
        detail,
        correction: correction.into(),
    }
}

/// The result of walking the parents of one path.
struct Parents {
    /// Index of the first component that does not exist, when one is missing.
    missing_from: Option<usize>,
    /// Preconditions for the existing parents.
    states: Vec<Precondition>,
    /// The removal this plan runs for a link at `missing_from`: the directories below it are
    /// created after that removal, so the link is not written through.
    retired_by: Option<String>,
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
    /// Directories the plan creates, by folded path: the spelling the first claim used. A second
    /// spelling that differs only by case would be one directory on a case-insensitive volume.
    mkdirs: BTreeMap<String, String>,
    /// Ledger entries this plan removes, by folded path: the removal operation id.
    retiring: BTreeMap<String, String>,
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
        mkdirs: BTreeMap::new(),
        retiring: removals(resolved, &cx.ledger, cx.scope)
            .into_iter()
            .map(|o| (fold(&o.path), removal_id(o)))
            .collect(),
    };
    for r in resolved.resources.values() {
        p.resource(r);
    }
    for o in removals(resolved, &cx.ledger, cx.scope) {
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
        scope: cx.scope.as_str().into(),
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
            correction: f.correction.into_owned(),
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
                        retired_by: None,
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
                Node::Link { to, target, .. } => {
                    // A link the manager owns and this plan removes is not written through:
                    // what is below it is planned as missing, after that removal.
                    if write && let Some(id) = self.retiring.get(&fold(&p)) {
                        return Ok(Parents {
                            missing_from: Some(i),
                            states,
                            retired_by: Some(id.clone()),
                        });
                    }
                    return Err(link_fail(field, &p, target, *to));
                }
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
            retired_by: None,
        })
    }

    fn resource(&mut self, r: &ResolvedResource) {
        if !self.cx.scope.covers(r) {
            self.out_of_scope(r);
            return;
        }
        if r.kind == "package" {
            self.package(r);
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
        let class = if r.sensitive {
            Class::Sensitive
        } else {
            Class::Normal
        };
        blank(id, (&r.id, &r.module, &r.adapter), class, path)
    }

    fn out_of_scope(&mut self, r: &ResolvedResource) {
        // A secret shows its destination and mode. Nothing else out of scope shows a path.
        let path = (r.kind == "secret").then(|| r.target.clone()).flatten();
        let effect = self.cx.scope.excuse(r);
        let id = format!("{}@scope", r.id);
        let mut op = self.skeleton(r, id.clone(), path);
        op.status = Status::NotInScope;
        op.effect = effect;
        // Only a secret declares a mode. A native document (git-config) may hold a `mode` table
        // of its own, which is not a file mode.
        if r.kind == "secret" {
            match declared_mode(r) {
                Ok(m) => op.mode = m,
                Err(f) => self.blocker(r, op.path.clone().as_deref(), f),
            }
        }
        self.ops.insert(id, op);
    }

    /// The source files that feed one claim, and the absolute link target when the claim is a
    /// link to a source.
    fn inputs(&self, r: &ResolvedResource, claim: &str) -> (Vec<Input>, Option<String>) {
        let wanted = |ptr: &str| match r.kind.as_str() {
            "distribution" => {
                let name = claim.rsplit('/').next().unwrap_or(claim);
                ptr == format!("/entries/{}/source", crate::resolve::pointer::escape(name))
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
        ledger_state(self.ledger_entry(path), sensitive)
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
        // The own-path check followed the parent link that this plan removes first, so it says
        // nothing about the directory that takes its place. The text scope check still ran.
        if parents.retired_by.is_none() {
            self.unchecked(claim, field)?;
        }
        op.preconditions.extend(parents.states.clone());
        let comps = parts(claim);
        let mut prev: Option<String> = parents.retired_by.clone();
        if let Some(from) = parents.missing_from {
            for i in from..comps.len() - 1 {
                prev = Some(self.mkdir(r, &comps[..=i].join("/"), prev.take(), field)?);
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
        // The ledger gives one path one owner. When another resource owns this path and the path
        // is missing, the plan retires that entry first, so apply never records two owners.
        let mut owner_state = self.ledger_state(claim, r.sensitive);
        let stale = if matches!(seen.node, Node::Missing {}) {
            self.ledger_entry(claim)
                .filter(|e| e.resource != r.id)
                .cloned()
        } else {
            None
        };
        if stale.is_some() {
            owner_state = ledger_state(None, r.sensitive);
        }
        op.preconditions.push(Precondition {
            subject: format!("ledger:{claim}"),
            state: owner_state,
        });
        let unowned = self.ledger_entry(claim).is_none();
        let (action, status, ownership, effect, post) = match r.kind.as_str() {
            "merge" => self.decide_merge(r, claim, &seen, op)?,
            "secret" => self.decide_secret(r, claim, &seen, op)?,
            _ => decide(r, claim, &seen, link, op.ownership, unowned)?,
        };
        // The decision succeeded, so the old owner's entry is retired first. The file is missing
        // here: a merge over a present file with another owner was refused in `decide_merge`.
        if let Some(old) = stale {
            self.remove(&old);
            op.after.push(removal_id(&old));
        }
        op.action = action;
        op.status = status;
        op.ownership = ownership;
        op.effect = effect;
        op.postcondition = post;
        op.link = link.map(String::from);
        Ok(())
    }

    /// A co-owned merge file (`claude-settings/v1`). The manager never owns the whole file, so
    /// the ledger fingerprint does not decide: the caller's verdict does. It read the live file
    /// and compared only the managed fields with the salted hash in the ledger. No live content
    /// reaches the plan, only the declared values' digest, the setting names and the verdict.
    fn decide_merge(
        &self,
        r: &ResolvedResource,
        path: &str,
        seen: &Seen,
        op: &mut Operation,
    ) -> Result<Decision, Fail> {
        let conflict = |what: String, fix: &'static str| fail(Code::Conflict, "target", what, fix);
        match &seen.node {
            Node::Unknown { reason } => return Err(unknown_fail("target", path, reason)),
            Node::Dir { .. } => {
                return Err(conflict(
                    format!("{path} is a directory where a settings file is needed"),
                    "move the directory aside; the manager never replaces a directory",
                ));
            }
            Node::Link { .. } | Node::Other { .. } => {
                return Err(conflict(
                    format!(
                        "{path} is a link or special file; the manager merges only a regular file"
                    ),
                    "replace it with a regular file that holds a JSON object, or remove it",
                ));
            }
            Node::File { own: false, .. } => {
                return Err(privilege("target", path, "is owned by another user"));
            }
            Node::File { mode, .. } if mode & 0o077 != 0 => {
                return Err(conflict(
                    format!(
                        "{path} is mode {mode:o}, open to group or others, and can hold credentials"
                    ),
                    "run chmod 600 on the file, then plan again; the manager does not change a mode it did not set",
                ));
            }
            _ => {}
        }
        let fact = self.cx.merge.get(&r.id);
        if let Some(f) = fact {
            op.managed_sha256 = Some(f.managed_sha256.clone());
            op.sets = f.sets.clone();
        }
        op.mode = Some(0o600);
        let what = fact.map_or_else(String::new, |f| {
            format!(
                ": {} setting(s), {} handler(s); live content is not shown",
                f.sets.len(),
                f.handlers
            )
        });
        let owned = self
            .ledger_entry(path)
            .is_some_and(|e| e.resource == r.id && e.coowned_entries.is_some());
        let post = format!(
            "{path} holds the declared managed values, seeds and handlers of {}",
            r.adapter
        );
        let shown = format!("{path} (sensitive: destination and mode only)");
        if let Some(MergeVerdict::Blocked {
            code,
            detail,
            correction,
        }) = fact.map(|f| &f.verdict)
        {
            return Err(fail(*code, "target", detail.clone(), correction.clone()));
        }
        // A merge into an existing file records this resource as owner, and the ledger allows one
        // owner per path, so the file is not written while another resource owns it. The CLI
        // blocks this earlier, but its peek can lose a race with a file that appears after it.
        // A missing file is fine: the leaf retires a stale owner's entry first.
        if !matches!(seen.node, Node::Missing {})
            && let Some(e) = self.ledger_entry(path).filter(|e| e.resource != r.id)
        {
            return Err(conflict(
                format!(
                    "{path} is recorded in the ledger as an output of {}",
                    e.resource
                ),
                "move the file aside, then plan again: the plan retires a stale entry and merges into a new file; if that resource is still selected, remove its claim on the path first",
            ));
        }
        if matches!(seen.node, Node::Missing {}) {
            return Ok((
                Action::CreateFile,
                Status::Planned,
                Ownership::Absent,
                format!("create {shown} by merge, mode 600{what}"),
                post,
            ));
        }
        match fact.map(|f| &f.verdict) {
            Some(MergeVerdict::Current) => Ok((
                Action::None,
                Status::Current,
                Ownership::Owned,
                format!("{shown} already holds the merge result{what}"),
                post,
            )),
            Some(MergeVerdict::Write) if !owned => {
                // The first write into an existing file the ledger does not record. The owner
                // approves it by id (`--allow-adopt`): hand edits of managed fields are lost.
                op.takeover = true;
                Ok((
                    Action::UpdateFile,
                    Status::Planned,
                    Ownership::Foreign,
                    format!(
                        "take over managed fields of an existing file {shown}; foreign keys are kept; hand edits of managed fields are overwritten; identical live hooks and append items become owned, and a later retire removes them{what}"
                    ),
                    post,
                ))
            }
            Some(MergeVerdict::Write) => Ok((
                Action::UpdateFile,
                Status::Planned,
                Ownership::Owned,
                format!(
                    "merge into {shown}: managed values set, seeds only if absent, owned handlers added; foreign keys and hooks stay{what}"
                ),
                post,
            )),
            // A blocked verdict returned above. No fact means the caller did not read the file.
            Some(MergeVerdict::Blocked { .. }) | None => Err(conflict(
                format!("{path} exists and its merge state was not read"),
                "run plan again; if it repeats, report the path",
            )),
        }
    }

    /// A package (docs/packages.md): no path and no ledger entry. The caller probed the declared
    /// provider; the plan installs what is missing and never upgrades, downgrades or removes.
    fn package(&mut self, r: &ResolvedResource) {
        let id = format!("{}@package", r.id);
        let mut op = self.skeleton(r, id.clone(), None);
        let name = r
            .value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&r.id)
            .to_string();
        let shown = format!("{name} with {}", r.adapter);
        let state = self.cx.package.get(&r.id).cloned();
        op.postcondition = format!("{} lists {name} as installed", r.adapter);
        let probe = format!("probe:{}:{name}", r.adapter);
        match state {
            Some(PackageState::Installed(v)) => {
                op.action = Action::None;
                op.status = Status::Current;
                op.effect = format!("{name} is installed by {} ({v})", r.adapter);
            }
            Some(PackageState::Missing(deps)) => {
                op.action = Action::Install;
                op.status = Status::Planned;
                let with = if deps.is_empty() {
                    String::new()
                } else {
                    format!(" and its missing dependencies {}", deps.join(", "))
                };
                op.effect = format!(
                    "install {shown}{with}: the provider's current versions; nothing installed is upgraded or downgraded"
                );
                op.preconditions.push(Precondition {
                    subject: probe,
                    state: PackageState::Missing(deps).state(),
                });
            }
            Some(PackageState::SourceBuild(names)) => {
                let f = fail(
                    Code::PlatformUnsupported,
                    "/name",
                    format!(
                        "{name} is not installed, and {} has no prebuilt binary for this machine with {}; a source build adds build-time dependencies the plan cannot show",
                        names.join(", "),
                        r.adapter
                    ),
                    "install it by hand with the provider, then run packages again",
                );
                op.effect = format!("blocked: {}", f.detail);
                self.blocker(r, None, f);
            }
            Some(PackageState::Upgrades(deps)) => {
                let f = fail(
                    Code::Conflict,
                    "/name",
                    format!(
                        "{name} is not installed, and installing it with {} would upgrade the installed dependencies {}; a package plan upgrades nothing",
                        r.adapter,
                        deps.join(", ")
                    ),
                    "upgrade those dependencies with the provider yourself, then run packages again",
                );
                op.effect = format!("blocked: {}", f.detail);
                self.blocker(r, None, f);
            }
            Some(PackageState::OtherOwner(why)) => {
                let f = fail(
                    Code::Conflict,
                    "/name",
                    format!(
                        "{name} is not installed by {} but {why}; a declared owner is not permission to replace another owner",
                        r.adapter
                    ),
                    "remove the other installation, or declare the adapter that owns it",
                );
                op.effect = format!("blocked: {}", f.detail);
                self.blocker(r, None, f);
            }
            Some(PackageState::Unsupported(why)) => {
                let f = fail(
                    Code::PrivilegeUnsupported,
                    "/name",
                    format!(
                        "{name} is not installed and {} cannot install it here: {why}",
                        r.adapter
                    ),
                    "install it by hand, then run packages again",
                );
                op.effect = format!("blocked: {}", f.detail);
                self.blocker(r, None, f);
            }
            Some(PackageState::Unknown(why)) => {
                let f = fail(
                    Code::ProbeFailed,
                    "/name",
                    format!("the installed state of {shown} is unknown: {why}"),
                    "make the provider answer its listing command, then run packages again",
                );
                op.effect = format!("blocked: {}", f.detail);
                self.blocker(r, None, f);
            }
            Some(PackageState::Refused(why)) => {
                let f = fail(
                    Code::ManifestInvalid,
                    "/name",
                    why,
                    format!("correct the resource value in {}", r.declared_in),
                );
                op.effect = format!("blocked: {}", f.detail);
                self.blocker(r, None, f);
            }
            None => {
                let f = fail(
                    Code::ProbeFailed,
                    "/name",
                    format!("{shown} was not probed"),
                    "run packages again; if it repeats, report the resource",
                );
                op.effect = format!("blocked: {}", f.detail);
                self.blocker(r, None, f);
            }
        }
        self.ops.insert(id, op);
    }

    /// A secret output (`op-template/v1`, docs/secrets.md). The content exists only after the
    /// provider renders it at apply time, so the plan never says `current`: every run renders
    /// and writes only when the result differs. The caller's verdict says whether the ledger
    /// proves the manager wrote the file that is there; if not, the overwrite is a takeover that
    /// needs `--allow-adopt`, as for a co-owned file.
    fn decide_secret(
        &self,
        r: &ResolvedResource,
        path: &str,
        seen: &Seen,
        op: &mut Operation,
    ) -> Result<Decision, Fail> {
        let conflict = |what: String, fix: &'static str| fail(Code::Conflict, "target", what, fix);
        match &seen.node {
            Node::Unknown { reason } => return Err(unknown_fail("target", path, reason)),
            Node::Dir { .. } => {
                return Err(conflict(
                    format!("{path} is a directory where a secret file is needed"),
                    "move the directory aside; the manager never replaces a directory",
                ));
            }
            Node::Link { .. } | Node::Other { .. } => {
                return Err(conflict(
                    format!(
                        "{path} is a link or special file; a secret is rendered only into a regular file"
                    ),
                    "replace it with a regular file, or remove it",
                ));
            }
            Node::File { own: false, .. } => {
                return Err(privilege("target", path, "is owned by another user"));
            }
            Node::File { mode, .. } if mode & 0o077 != 0 => {
                return Err(conflict(
                    format!("{path} is mode {mode:o}, open to group or others, and holds a secret"),
                    "run chmod 600 on the file, then plan again; the manager does not change a mode it did not set",
                ));
            }
            _ => {}
        }
        let mode = op.mode.unwrap_or(0o600);
        let shown = format!("{path} (secret: destination and mode only)");
        let post = format!(
            "{path} holds the rendered template of {}, mode {mode:o}",
            r.adapter
        );
        if let Some(e) = self.ledger_entry(path).filter(|e| e.resource != r.id)
            && !matches!(seen.node, Node::Missing {})
        {
            return Err(conflict(
                format!(
                    "{path} is recorded in the ledger as an output of {}",
                    e.resource
                ),
                "move the file aside, then plan again; the manager does not render into another resource's output",
            ));
        }
        if matches!(seen.node, Node::Missing {}) {
            return Ok((
                Action::CreateFile,
                Status::Planned,
                Ownership::Absent,
                format!("render {shown}, mode {mode:o}; the content is never shown"),
                post,
            ));
        }
        match self.cx.secret.get(&r.id) {
            Some(SecretVerdict::Owned) => Ok((
                Action::UpdateFile,
                Status::Planned,
                Ownership::Owned,
                format!("re-render {shown}; the file is written only when the result differs"),
                post,
            )),
            Some(SecretVerdict::Unowned) => {
                op.takeover = true;
                Ok((
                    Action::UpdateFile,
                    Status::Planned,
                    Ownership::Foreign,
                    format!(
                        "take over {shown}: the ledger does not prove the manager wrote it; the approval overwrites it with the rendered template"
                    ),
                    post,
                ))
            }
            Some(SecretVerdict::Blocked {
                code,
                detail,
                correction,
            }) => Err(fail(*code, "target", detail.clone(), correction.clone())),
            None => Err(conflict(
                format!("{path} exists and its state was not read"),
                "run plan again; if it repeats, report the path",
            )),
        }
    }

    /// A ledger output that nothing selects any more. It is removed only when the ledger proves
    /// the live output is the one the manager wrote. Anything else is a conflict.
    fn remove(&mut self, o: &Owned) {
        let id = removal_id(o);
        self.removal_ids.insert(id.clone());
        let mut op = blank(
            id.clone(),
            (&o.resource, "", &o.adapter),
            o.class,
            Some(o.path.clone()),
        );
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
        // A link in a parent is reported first and for this entry only. Such an entry (a
        // stow-style linked config directory, for example) must not block any other operation.
        let comps = parts(path);
        for i in 0..comps.len().saturating_sub(1) {
            let p = comps[..=i].join("/");
            if let Some(Seen {
                node: Node::Link { target, to, .. },
                ..
            }) = self.obs.seen.get(&p)
            {
                return Err(link_fail(field, &p, target, *to));
            }
        }
        self.claim_checks(path, field)?;
        let parents = self.parents(path, field, true)?;
        self.unchecked(path, field)?;
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
                // Apply drops the ledger's stale entry for it (docs/apply.md).
                op.action = Action::Remove;
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
                    "move the directory aside; the next plan sees the path absent and drops the stale ledger entry",
                ));
            }
            _ => {}
        }
        // A secret output (docs/secrets.md) is never removed and never retired. The file and its
        // ledger entry stay until the user deletes them by hand.
        if o.class == Class::Sensitive && o.adapter == SECRET_ADAPTER {
            op.action = Action::None;
            op.status = Status::Current;
            op.ownership = Ownership::Owned;
            op.effect = format!(
                "{path} is a secret output of a deselected resource; kept with its ledger entry. The manager never removes a secret: delete the file by hand if you want it gone"
            );
            op.postcondition = format!("{path} is left as it is");
            return Ok(());
        }
        if let (Some((n, items)), Node::File { .. }) = (o.coowned_entries, &seen.node) {
            // A co-owned file is never deleted. Only the entries the manager added are retired.
            op.action = Action::Retire;
            op.status = Status::Planned;
            op.ownership = Ownership::Owned;
            op.mode = Some(0o600);
            op.effect = format!(
                "retire {n} owned handler(s) and remove {items} appended array item(s) in {path} (sensitive: destination and mode only); the file, its managed and seeded values and all foreign entries and array items stay"
            );
            op.postcondition =
                format!("{path} holds none of the owned handler entries and appended items");
            return Ok(());
        }
        op.ownership = self.ownership(&o.resource, o.class == Class::Sensitive, path, &seen);
        let why = match op.ownership {
            Ownership::Owned => None,
            Ownership::Modified => Some("was changed after the manager wrote it"),
            Ownership::Unverifiable => Some("has an owner the manager cannot verify"),
            _ => Some("is not owned by this resource according to the ledger"),
        };
        if let Some(why) = why {
            let fix = if op.ownership == Ownership::Modified {
                "if an apply was interrupted, run `dotfiles-manager state recover` first. Otherwise review the file and remove it yourself; the manager removes only what it can prove it wrote"
            } else {
                "review the file and remove it yourself; the manager removes only what it can prove it wrote"
            };
            return Err(fail(
                Code::Conflict,
                field,
                format!("{path} is no longer selected but {why}"),
                fix,
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
    fn mkdir(
        &mut self,
        r: &ResolvedResource,
        path: &str,
        parent: Option<String>,
        field: &'static str,
    ) -> Result<String, Fail> {
        let first = self.mkdirs.entry(fold(path)).or_insert_with(|| path.into());
        if first != path {
            return Err(fail(
                Code::TargetCollision,
                field,
                format!(
                    "the missing parent {path} differs from the directory `{first}` another claim creates only by case; the filesystem treats them as one"
                ),
                "spell both claims with the same case",
            ));
        }
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
        Ok(id)
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
    fn fragment_problem(&self, dir: &str, name: &str, sensitive: bool) -> Option<Fail> {
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
            // A fragment of a sensitive merge sets keys that run code, so another user must not
            // be able to write it. It is not hashed: the CLI supplies a salted content token.
            Some((
                Node::File {
                    own: true, mode, ..
                },
                None,
            )) if sensitive && mode & 0o022 != 0 => Some(fail(
                Code::Conflict,
                field,
                format!(
                    "fragment {path} is mode {mode:o}, writable by group or others, and its keys can run code"
                ),
                "run chmod go-w on the fragment",
            )),
            Some((Node::File { own: true, .. }, None)) if sensitive => None,
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
        // The fragments of a sensitive merge can hold credentials. They are not hashed, and the
        // plan carries a salted content token from the CLI instead of a hash.
        let sens = r.sensitive;
        let selected = self.producers(&d.dir, &d.pattern);
        let selected_names: BTreeSet<String> = selected.iter().map(|(_, _, n)| fold(n)).collect();
        let mut fragments: Vec<Fragment> = selected
            .iter()
            .map(|(_, res, n)| Fragment {
                name: n.clone(),
                origin: format!("selected:{res}"),
                sha256: None,
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
                // A fragment that a planned removal deletes is not an input. It leaves the list,
                // and the consumer runs after the removal, so deselected content never reaches
                // the assembled file and one apply converges.
                if let Some((rid, _)) = removals.get(&fold(&path)) {
                    removing.push((n.clone(), rid.clone()));
                    continue;
                }
                let sha256 = match self.obs.seen.get(&path).map(|s| &s.node) {
                    Some(Node::File { sha256, .. }) if !sens => sha256.clone(),
                    _ => None,
                };
                fragments.push(Fragment {
                    name: n.clone(),
                    origin: "foreign".into(),
                    sha256,
                });
                let st = self
                    .obs
                    .seen
                    .get(&path)
                    .map_or_else(|| "unobserved".into(), |s| node_state(s, sens));
                foreign_state.push(format!("{n}={st}"));
                if let Some(f) = self.fragment_problem(&d.dir, n, sens) {
                    problems.push((path, f));
                }
            }
        }
        fragments.sort_by(|a, b| (&a.name, &a.origin).cmp(&(&b.name, &b.origin)));
        let mut names: Vec<String> = selected.iter().map(|(_, _, n)| n.clone()).collect();
        names.sort();
        let mut state = format!(
            "selected=[{}] foreign=[{}] removing=[{}]",
            names.join(","),
            foreign_state.join(","),
            removing
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(",")
        );
        // The fragments the merge read must be the fragments the planner lists. They differ when
        // a deselected producer's fragment is not removed (a removal that is blocked): the merge
        // verdict was made without it and the plan lists it, so neither can be trusted.
        let fact = self
            .cx
            .merge
            .get(&r.id)
            .filter(|f| sens && !matches!(f.verdict, MergeVerdict::Blocked { .. }));
        if let Some(f) = fact {
            let mut read: Vec<&str> = f.fragments.iter().map(|(n, _)| n.as_str()).collect();
            let mut listed: Vec<&str> = fragments.iter().map(|x| x.name.as_str()).collect();
            read.sort_unstable();
            listed.sort_unstable();
            if read != listed {
                problems.push((
                    d.dir.clone(),
                    fail(
                        Code::PlanStale,
                        "/fragments_dir",
                        format!(
                            "the fragments in {} that the merge read differ from the fragments the plan lists; a fragment of a deselected resource is probably still there",
                            d.dir
                        ),
                        "settle the blocked removal of that fragment (see the other blocker), then plan again",
                    ),
                ));
            }
            let tokens: Vec<String> = f
                .fragments
                .iter()
                .map(|(n, t)| format!("{n}:{t}"))
                .collect();
            state.push_str(&format!(" content=[{}]", tokens.join(",")));
        }
        let blocked = !problems.is_empty();
        for (path, f) in problems {
            self.blocker(r, Some(&path), f);
        }
        for cid in &consumers {
            if let Some(op) = self.ops.get_mut(cid) {
                let waits = selected
                    .iter()
                    .map(|(pid, _, _)| pid)
                    .chain(removing.iter().map(|(_, rid)| rid));
                for pid in waits {
                    if !op.after.contains(pid) {
                        op.after.push(pid.clone());
                    }
                }
                op.after.sort();
                op.fragments = fragments.clone();
                op.fragment_dir = Some(d.dir.clone());
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

/// The id of the operation that removes one ledger output.
fn removal_id(o: &Owned) -> String {
    format!("remove@{}@{}", o.resource, o.path)
}

/// An operation with nothing decided: blocked, no action. `who` is the resource, module and
/// adapter ids.
fn blank(id: String, who: (&str, &str, &str), class: Class, path: Option<String>) -> Operation {
    Operation {
        id,
        journal_id: String::new(),
        resource: who.0.into(),
        module: who.1.into(),
        adapter: who.2.into(),
        class,
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
        fragment_dir: None,
        output_sha256: None,
        managed_sha256: None,
        takeover: false,
        output_bytes: None,
        excluded: None,
        sets: Vec::new(),
        after: Vec::new(),
        preconditions: Vec::new(),
        postcondition: "none".into(),
    }
}

type Decision = (Action, Status, Ownership, String, String);

fn decide(
    r: &ResolvedResource,
    path: &str,
    seen: &Seen,
    link: Option<&str>,
    ownership: Ownership,
    unowned: bool,
) -> Result<Decision, Fail> {
    // Only a normal-class output that no ledger entry mentions can be adopted.
    let adoptable = unowned && !r.sensitive;
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
    let modified = || {
        fail(
            Code::Conflict,
            "target",
            format!("{path} was changed after the manager wrote it"),
            "if an apply was interrupted, run `dotfiles-manager state recover` first. Otherwise review the file, then move it aside or adopt it; the manager does not overwrite content it does not own",
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
                // An exact link to a file that no ledger entry mentions: ownership is recorded
                // and nothing is written. A link to a directory is never adopted.
                (
                    Node::Link {
                        target,
                        to: LinkTo::File,
                        ..
                    },
                    _,
                ) if target == want && adoptable => Ok((
                    Action::Adopt,
                    Status::Planned,
                    Ownership::UnrecordedCurrent,
                    format!("adopt {shown}: record the existing link -> {want}, write nothing"),
                    post,
                )),
                (Node::Link { target, to, .. }, o) if target == want => {
                    // `current` must not read as owned: say why an exact link is left alone.
                    let (own, effect) = if o == Ownership::Owned {
                        (
                            Ownership::Owned,
                            format!("link {shown} -> {want} already holds"),
                        )
                    } else {
                        let why = if r.sensitive {
                            "a sensitive output is never adopted".to_string()
                        } else if !unowned {
                            "the ledger already has an entry for this path".to_string()
                        } else {
                            format!("it resolves to {}, not a file", to.name())
                        };
                        (
                            Ownership::UnrecordedCurrent,
                            format!("link {shown} -> {want} already holds; not adopted: {why}"),
                        )
                    };
                    Ok((Action::None, Status::Current, own, effect, post))
                }
                (Node::Link { .. }, Ownership::Owned) => Ok((
                    Action::ReplaceLink,
                    Status::Planned,
                    Ownership::Owned,
                    format!("replace {shown} with a link -> {want}"),
                    post,
                )),
                // The ledger owns a file here (the resource was a render before). A link
                // replacement over it would fail at apply, after earlier operations ran.
                (_, Ownership::Owned) => Err(fail(
                    Code::Conflict,
                    "target",
                    format!(
                        "{path} is a file where this resource now creates a link; the manager never replaces a file with a link"
                    ),
                    "remove the file yourself, then plan again to create the link",
                )),
                (_, Ownership::Modified) => Err(modified()),
                (_, Ownership::Unverifiable) => {
                    Err(conflict("has an owner the manager cannot verify"))
                }
                (Node::File { .. }, _) => Err(fail(
                    Code::Conflict,
                    "target",
                    format!(
                        "{path} is a regular file, not the link -> {want} this resource would create; the manager adopts only an exact existing link"
                    ),
                    "move the file aside, or replace it with that link by hand",
                )),
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
                (Node::File { .. }, Ownership::Owned) => Ok((
                    Action::UpdateFile,
                    Status::Planned,
                    Ownership::Owned,
                    format!("update {shown} with {}", r.adapter),
                    post,
                )),
                // The ledger owns a link here (the resource was a link before). A file update
                // must not write through it or replace it.
                (_, Ownership::Owned) => Err(fail(
                    Code::Conflict,
                    "target",
                    format!(
                        "{path} is a link where this resource now generates a file; the manager never updates a file through a link"
                    ),
                    "remove the link yourself, then plan again to create the file",
                )),
                (_, Ownership::Modified) => Err(modified()),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(sensitive: bool) -> ResolvedResource {
        ResolvedResource {
            id: "core.ssh".into(),
            module: "core".into(),
            declared_in: "m.toml".into(),
            kind: "link".into(),
            adapter: "fs-link/v1".into(),
            target: Some(".ssh/config".into()),
            value: Value::Table(Default::default()),
            defaults: vec![],
            extensions: vec![],
            secret_values: false,
            sensitive,
            native_document: false,
            provenance: BTreeMap::new(),
            claims: vec![".ssh/config".into()],
            dir_claims: vec![],
            precedence: None,
        }
    }

    fn of_kind(kind: &str) -> ResolvedResource {
        ResolvedResource {
            kind: kind.into(),
            ..resource(kind == "secret")
        }
    }

    #[test]
    fn each_scope_covers_its_kinds_and_excuses_the_others() {
        for kind in ["link", "render", "merge", "assemble", "distribution"] {
            assert!(Scope::Config.covers(&of_kind(kind)), "{kind}");
            assert!(!Scope::Secrets.covers(&of_kind(kind)), "{kind}");
            assert!(!Scope::Packages.covers(&of_kind(kind)), "{kind}");
            assert_eq!(
                Scope::Secrets.excuse(&of_kind(kind)),
                format!("{kind} is configuration, outside a secrets plan")
            );
        }
        assert!(Scope::Secrets.covers(&of_kind("secret")));
        assert!(Scope::Packages.covers(&of_kind("package")));
        for scope in [Scope::Config, Scope::Secrets, Scope::Packages] {
            assert!(!scope.covers(&of_kind("application")), "{scope:?}");
        }
        // The configuration plan keeps the original secret text; other scopes say which plan.
        assert_eq!(
            Scope::Config.excuse(&of_kind("secret")),
            "secret output: destination and mode only; rendering needs the secrets command"
        );
        assert!(
            Scope::Packages
                .excuse(&of_kind("secret"))
                .ends_with(", outside a packages plan")
        );
        assert_eq!(
            Scope::Config.excuse(&of_kind("package")),
            "package is provisioning, outside a config plan"
        );
        assert_eq!(Scope::Packages.as_str(), "packages");
    }

    #[test]
    fn a_package_state_has_one_precondition_string_per_variant() {
        let states = [
            PackageState::Installed("1.2".into()),
            PackageState::Missing(Vec::new()),
            PackageState::Missing(vec!["aom".into(), "libheif".into()]),
            PackageState::Upgrades(vec!["libpng".into()]),
            PackageState::SourceBuild(vec!["afuse".into()]),
            PackageState::OtherOwner("x".into()),
            PackageState::Unsupported("root".into()),
            PackageState::Unknown("timeout".into()),
            PackageState::Refused("bad name".into()),
        ];
        let strings: BTreeSet<String> = states.iter().map(PackageState::state).collect();
        assert_eq!(strings.len(), states.len());
        assert_eq!(PackageState::Missing(Vec::new()).state(), "missing");
        assert_eq!(states[2].state(), "missing:with=aom,libheif");
        // A dependency an earlier install added is not a stale plan; a new one is.
        let want = states[2].state();
        assert!(PackageState::Missing(vec!["libheif".into()]).still_planned(&want));
        assert!(PackageState::Missing(Vec::new()).still_planned(&want));
        assert!(!PackageState::Missing(vec!["x265".into()]).still_planned(&want));
        assert!(!PackageState::Missing(vec!["aom".into()]).still_planned("missing"));
        assert!(PackageState::Missing(Vec::new()).still_planned("missing"));
        assert!(!PackageState::Upgrades(vec!["aom".into()]).still_planned(&want));
        assert_eq!(states[0].state(), "installed:1.2");
    }

    fn link(to: LinkTo) -> Seen {
        Seen {
            node: Node::Link {
                target: "/r/x".into(),
                to,
                own: true,
            },
            alias: None,
        }
    }

    fn run(sensitive: bool, seen: &Seen, ownership: Ownership, unowned: bool) -> Decision {
        decide(
            &resource(sensitive),
            ".ssh/config",
            seen,
            Some("/r/x"),
            ownership,
            unowned,
        )
        .unwrap_or_else(|f| panic!("{}", f.detail))
    }

    #[test]
    fn an_exact_unowned_link_to_a_file_is_adopted() {
        let d = run(false, &link(LinkTo::File), Ownership::Foreign, true);
        assert_eq!(
            (d.0, d.1, d.2),
            (Action::Adopt, Status::Planned, Ownership::UnrecordedCurrent)
        );
    }

    #[test]
    fn an_exact_link_that_is_not_adoptable_stays_current_and_says_why() {
        let cases = [
            (
                "sensitive",
                run(true, &link(LinkTo::File), Ownership::Foreign, true),
                "a sensitive output is never adopted",
            ),
            (
                "other owner",
                run(false, &link(LinkTo::File), Ownership::Foreign, false),
                "the ledger already has an entry for this path",
            ),
            (
                "directory",
                run(false, &link(LinkTo::Dir), Ownership::Foreign, true),
                "it resolves to dir, not a file",
            ),
        ];
        for (what, d, why) in cases {
            assert_eq!(
                (d.0, d.1, d.2),
                (Action::None, Status::Current, Ownership::UnrecordedCurrent),
                "{what}"
            );
            assert!(
                d.3.ends_with(&format!("already holds; not adopted: {why}")),
                "{what}: {}",
                d.3
            );
        }
        // A ledger entry written before the fingerprint prefix reads as modified. The exact
        // link is still current and is not adopted, replaced or removed.
        let d = run(false, &link(LinkTo::File), Ownership::Modified, false);
        assert_eq!(
            (d.0, d.1, d.2),
            (Action::None, Status::Current, Ownership::UnrecordedCurrent)
        );
        assert!(
            d.3.ends_with("not adopted: the ledger already has an entry for this path"),
            "{}",
            d.3
        );
        // An owned link is plain current.
        let d = run(false, &link(LinkTo::File), Ownership::Owned, false);
        assert_eq!((d.0, d.2), (Action::None, Ownership::Owned));
        assert!(!d.3.contains("not adopted"), "{}", d.3);
    }
}
