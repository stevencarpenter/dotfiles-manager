//! Apply: executes a plan's operations against HOME (docs/apply.md).
//!
//! [`apply`] is the one entry point. It checks the gates that need no write ([`gates`]), settles
//! interrupted work and then runs the operations in the plan's order. The caller re-plans under
//! the manager lock and compares the approved digest before it gets here. For each operation and
//! immediately before its effect, this module re-checks every fact the plan recorded (`path:`
//! and `ledger:` preconditions) and the topology of the parent chain, walked from the opened HOME
//! one component at a time without following a link. The effect itself uses a primitive that
//! cannot replace or follow what it did not verify (`crate::publish`).
//!
//! Order of one operation: prerequisites, intent entry, effect, read-back, outcome entry, ledger
//! update. The receipt is written by the caller after the last operation.

use crate::fail::Fail;
use crate::fsys::{Dir, Live, Peek, Sys};
use crate::generate::{Completion, Gather, Generator, GeneratorInputs, gather, link_source};
use crate::observe::observe;
use crate::publish::Publish;
use crate::recover;
use crate::store::{Store, now_ns};
use dfm_model::digest::sha256_hex;
use dfm_model::fold;
use dfm_model::merge::{Merged, check_foreign, leaf_hashes, salted};
use dfm_model::plan::{
    Action, Node, Operation, Owned, PackageState, Plan, Request, ServiceState,
    Status as PlanStatus, file_fingerprint, ledger_state, link_fingerprint, node_state,
};
use dfm_model::state::{
    Class, CoOwned, Fingerprint, JournalEntry, Ledger, OperationRecord, OwnedOutput, Phase,
    Recovery, RecoveryOutput, SCHEMA, Status, rfc3339, valid_id,
};
use dfm_model::{Code, Refusal};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// A published file is at most this large (the same bound the planner hashes).
const MAX_FILE: u64 = dfm_model::plan::MAX_HASHED_BYTES;

/// A conservative size for the two journal entries (intent and outcome) of one operation. An
/// entry is bounded to 64 KiB, and apply writes reasons of at most 1024 characters.
const JOURNAL_BYTES_PER_OPERATION: u64 = 4096;

/// The consumer reload state recorded for every published output: a published file never
/// implies a reloaded consumer. A service operation records [`RELOAD_RELOADED`] or
/// [`RELOAD_REQUESTED`] instead (docs/lifecycle.md).
pub const RELOAD_NOT_ATTEMPTED: &str = "not_attempted";
/// The launcher reports the consumer loaded as declared after the start or restart.
pub const RELOAD_RELOADED: &str = "reloaded";
/// The launcher accepted the request; completion was not observed within the bound (`pending`).
pub const RELOAD_REQUESTED: &str = "requested";

pub struct Run<'a> {
    pub store: &'a Store,
    pub sys: Sys,
    /// The opened HOME. Every effect is relative to it.
    pub home: &'a Dir,
    /// HOME as the layout spells it. Ledger and journal paths use this spelling.
    pub home_path: &'a Path,
    pub generator: &'a dyn Generator,
    /// Operation ids the caller approved as disruptive (`--allow-disruptive`). The set must
    /// equal the set of planned removals: the plan determines it.
    pub allow: &'a BTreeSet<String>,
    /// Operation ids the caller approved as adoptions (`--allow-adopt`). The set must equal the
    /// set of planned adoptions.
    pub allow_adopt: &'a BTreeSet<String>,
    pub fail: &'a Fail,
}

/// What happened to every operation of the plan, in plan order.
#[derive(Debug)]
pub struct Report {
    pub operations: Vec<OperationRecord>,
    /// The first operation failure or block, with its own code. Later operations are blocked or
    /// not started. The caller marks it with `after_mutation` when the store wrote anything.
    pub stopped: Option<Refusal>,
    /// Bytes of a torn final journal line that were cut off before settling (0 when none).
    pub repaired: usize,
    /// Staged copies of an interrupted publish that settling could not remove.
    pub warnings: Vec<String>,
}

/// The result of the gates that need no write.
#[derive(Debug)]
pub struct Gates {
    /// Ids of the planned removals, in plan order. Each needs `--allow-disruptive <id>`.
    pub removals: Vec<String>,
    /// Ids of the planned adoptions, in plan order. Each needs `--allow-adopt <id>`.
    pub adoptions: Vec<String>,
    /// The first gate that refuses the plan (a blocker, a malformed plan, a file with no
    /// generator, a sensitive output), if any.
    pub refusal: Option<Refusal>,
}

impl Gates {
    /// The planned operation ids that need this kind of approval.
    pub fn planned(&self, kind: Approval) -> &[String] {
        match kind {
            Approval::Disruptive => &self.removals,
            Approval::Adopt => &self.adoptions,
        }
    }
}

/// Converts the ledger into the planner's view: paths relative to HOME.
pub fn owned_outputs(ledger: &Ledger, home: &Path) -> Result<Vec<Owned>, Refusal> {
    let mut out = Vec::new();
    for r in &ledger.resources {
        for o in &r.owned_outputs {
            let rel = Path::new(&o.path)
                .strip_prefix(home)
                .ok()
                .and_then(Path::to_str)
                .ok_or_else(|| {
                    Refusal::new(
                        Code::StateInvalid,
                        format!("ledger path {} is outside HOME", o.path),
                    )
                })?;
            out.push(Owned {
                resource: r.resource.clone(),
                adapter: r.adapter.clone(),
                class: r.class,
                path: rel.into(),
                fingerprint: o.fingerprint.clone(),
                coowned_entries: o
                    .coowned
                    .as_ref()
                    .map(|c| (c.entries.len(), c.appended.len())),
            });
        }
    }
    Ok(out)
}

fn plan_invalid(detail: impl Into<String>) -> Refusal {
    Refusal::new(Code::PlanInvalid, detail)
}

/// The components of a HOME-relative path. A component that is empty, `.`, `..` or holds a
/// control character is refused: the plan is ours, but the check costs nothing.
pub(crate) fn components(rel: &str) -> Result<Vec<&str>, Refusal> {
    let comps: Vec<&str> = rel.split('/').collect();
    if comps
        .iter()
        .any(|c| c.is_empty() || *c == "." || *c == ".." || c.chars().any(char::is_control))
    {
        return Err(plan_invalid(format!(
            "{rel:?} is not a normalized HOME-relative path"
        )));
    }
    Ok(comps)
}

/// Whether the action publishes through a same-directory staged name.
fn stages(a: Action) -> bool {
    matches!(
        a,
        Action::CreateFile
            | Action::UpdateFile
            | Action::ReplaceLink
            | Action::AdoptFile
            | Action::Retire
    )
}

/// The `file_fingerprint` of the regular file an `adopt_file` operation replaces: the hash its
/// `path:` precondition recorded when `dfm adopt` planned it.
fn adopted_fingerprint<'a>(op: &'a Operation, rel: &str) -> Option<&'a str> {
    op.preconditions
        .iter()
        .find(|p| p.subject.strip_prefix("path:") == Some(rel))?
        .state
        .strip_prefix("file:")?
        .split_once(":sha256:")
        .map(|(_, h)| h)
}

fn is_file_action(a: Action) -> bool {
    matches!(a, Action::CreateFile | Action::UpdateFile)
}

/// The two approvals a plan can need before apply writes: each names its operations by id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    /// `--allow-disruptive`: deleting an owned output, or restarting a service
    /// (docs/apply.md, docs/lifecycle.md).
    Disruptive,
    /// `--allow-adopt`: recording ownership of an existing link (DFM-45).
    Adopt,
}

impl Approval {
    pub const ALL: [Approval; 2] = [Approval::Disruptive, Approval::Adopt];

    pub fn from_flag(flag: &str) -> Option<Approval> {
        Approval::ALL.into_iter().find(|a| a.flag() == flag)
    }

    /// The approval an operation needs: a removal or a service restart is disruptive, an
    /// adoption and the first write into an existing co-owned file (`takeover`) are adoptions.
    pub fn of(op: &Operation) -> Option<Approval> {
        if op.takeover {
            return Some(Approval::Adopt);
        }
        match op.action {
            Action::Remove | Action::Restart => Some(Approval::Disruptive),
            Action::Adopt | Action::AdoptFile => Some(Approval::Adopt),
            _ => None,
        }
    }

    pub fn flag(self) -> &'static str {
        match self {
            Approval::Disruptive => "--allow-disruptive",
            Approval::Adopt => "--allow-adopt",
        }
    }

    pub fn noun(self) -> &'static str {
        match self {
            Approval::Disruptive => "removal or restart",
            Approval::Adopt => "adoption",
        }
    }

    /// The refusal for a planned operation without its approval.
    pub fn code(self) -> Code {
        match self {
            Approval::Disruptive => Code::DisruptiveUnapproved,
            Approval::Adopt => Code::AdoptUnapproved,
        }
    }

    /// The `how` of a receipt `approvals[]` entry.
    pub fn how(self) -> &'static str {
        match self {
            Approval::Disruptive => "allow_disruptive",
            Approval::Adopt => "allow_adopt",
        }
    }
}

/// `--allow-disruptive '<id>'` or `--allow-adopt '<id>'`, quoted so a user can paste it into a
/// shell. The id is raw: the command escapes the whole refusal once when it prints it, and a
/// resolved target holds no control or backslash character, so the printed id is the real one.
pub fn flag(kind: Approval, id: &str) -> String {
    format!("{} '{}'", kind.flag(), id.replace('\'', "'\\''"))
}

/// A link target must lie inside the root of the operation's own source: the root whose
/// `<root>/<first input path>` is the target. A target outside every source root, or one that
/// names a different file than the operation's input, is refused.
fn link_inside_its_source(plan: &Plan, op: &Operation, link: &str) -> bool {
    if link.contains('\0') || link.split('/').any(|c| c == "..") {
        return false;
    }
    plan.sources.iter().any(|s| {
        let inside = link
            .strip_prefix(s.root.as_str())
            .is_some_and(|rest| rest.starts_with('/'));
        inside
            && op
                .inputs
                .first()
                .is_none_or(|i| link == format!("{}/{}", s.root, i.path))
    })
}

/// The gates that need no write. They are pure, so a preview can run them. A refusal means
/// nothing was changed (exit 3).
pub fn gates(plan: &Plan, generator: &dyn Generator) -> Gates {
    let planned = |kind: Approval| {
        plan.operations
            .iter()
            .filter(|o| o.status == PlanStatus::Planned && Approval::of(o) == Some(kind))
            .map(|o| o.id.clone())
            .collect()
    };
    Gates {
        removals: planned(Approval::Disruptive),
        adoptions: planned(Approval::Adopt),
        refusal: first_refusal(plan, generator).err(),
    }
}

fn first_refusal(plan: &Plan, generator: &dyn Generator) -> Result<(), Refusal> {
    if let Some(b) = plan.blockers.first() {
        return Err(b.refusal());
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for op in &plan.operations {
        if !seen.insert(&op.id) {
            return Err(plan_invalid(format!("operation {} appears twice", op.id)));
        }
        if let Some(missing) = op.after.iter().find(|a| !seen.contains(a.as_str())) {
            return Err(plan_invalid(format!(
                "{} runs after {missing}, which does not come earlier in the plan",
                op.id
            )));
        }
        if !valid_id(&op.journal_id) {
            return Err(plan_invalid(format!("{} has no valid journal id", op.id)));
        }
        if op.status == PlanStatus::Blocked {
            return Err(plan_invalid(format!(
                "{} is blocked but the plan lists no blocker",
                op.id
            )));
        }
        if op.status != PlanStatus::Planned {
            continue;
        }
        // A package install or upgrade, or a service start or restart, has no path and no
        // content: it needs a provider in this build, and a probe precondition to compare under
        // the lock.
        if matches!(
            op.action,
            Action::Install | Action::Upgrade | Action::Start | Action::Restart
        ) {
            let (what, provided, missing) =
                if matches!(op.action, Action::Install | Action::Upgrade) {
                    (
                        "installs a package",
                        generator.installs(op),
                        "no package provider for",
                    )
                } else {
                    (
                        "acts on a service",
                        generator.reloads(op),
                        "no service launcher for",
                    )
                };
            if op.path.is_some() || op.class != Class::Normal {
                return Err(plan_invalid(format!(
                    "{} {what} but carries a path or is sensitive",
                    op.id
                )));
            }
            if !provided {
                return Err(Refusal::new(
                    Code::GeneratorUnavailable,
                    format!(
                        "{}: {missing} {} in this build; nothing was changed",
                        op.id, op.adapter
                    ),
                ));
            }
            if !op
                .preconditions
                .iter()
                .any(|p| p.subject.starts_with("probe:"))
            {
                return Err(plan_invalid(format!(
                    "{} {what} but records no probe precondition",
                    op.id
                )));
            }
            continue;
        }
        let path = op
            .path
            .as_deref()
            .ok_or_else(|| plan_invalid(format!("{} has no path", op.id)))?;
        let comps = components(path)?;
        // A file or link is staged under a longer hidden name in its own directory. A leaf that
        // leaves no room for that name would fail with `ENAMETOOLONG` after the intent entry.
        // Every other action still needs a name the file system accepts.
        let leaf = comps.last().map_or(0, |l| l.len());
        let extra = if stages(op.action) {
            crate::fsys::TEMP_NAME_EXTRA
        } else {
            0
        };
        if leaf + extra > crate::fsys::NAME_MAX {
            let staged = if extra > 0 {
                format!("the staged name needs up to {extra} more and ")
            } else {
                String::new()
            };
            return Err(Refusal::new(
                Code::LimitExceeded,
                format!(
                    "{}: the file name is {leaf} bytes; {staged}a name holds {}. Nothing was changed",
                    op.id,
                    crate::fsys::NAME_MAX
                ),
            ));
        }
        // A sensitive output has no content hash. Apply admits two kinds of sensitive operation:
        // the co-owned merge, which has its own conflict detection (docs/apply.md, Merge), and a
        // rendered secret (docs/secrets.md). Any other sensitive operation, a directory included,
        // refuses the whole plan.
        let merge = op.class == Class::Sensitive && generator.merges(op);
        let render = op.class == Class::Sensitive && !merge && generator.renders(op);
        if op.class == Class::Sensitive && !merge && !render {
            return Err(Refusal::new(
                Code::SensitiveUnsupported,
                format!(
                    "{}: this sensitive output is not published or removed by this build (only a co-owned merge and an op-template/v1 secret are)",
                    op.id
                ),
            ));
        }
        if render {
            if !is_file_action(op.action)
                || op.output_sha256.is_some()
                || op.managed_sha256.is_some()
                || op.inputs.len() != 1
            {
                return Err(plan_invalid(format!(
                    "{} renders a secret but is not a file write with one template and no content hash",
                    op.id
                )));
            }
            continue;
        }
        if merge {
            if op.action != Action::Retire && op.managed_sha256.is_none() {
                return Err(plan_invalid(format!(
                    "{} is a merge but the plan records no managed fingerprint",
                    op.id
                )));
            }
            if op.action == Action::Retire && op.managed_sha256.is_some() {
                return Err(plan_invalid(format!(
                    "{} retires entries but carries a managed fingerprint",
                    op.id
                )));
            }
            continue;
        }
        match op.action {
            Action::CreateLink | Action::ReplaceLink | Action::Adopt | Action::AdoptFile => {
                let link = op
                    .link
                    .as_deref()
                    .ok_or_else(|| plan_invalid(format!("{} has no link target", op.id)))?;
                if !link_inside_its_source(plan, op, link) {
                    return Err(plan_invalid(format!(
                        "{} links to {link:?}, which is not inside the root of its own source",
                        op.id
                    )));
                }
                // The file is replaced only while it and the source still hold the bytes adopt
                // compared: both hashes must be in the plan, because the publisher checks both
                // again right before the rename.
                if op.action == Action::AdoptFile {
                    let hashed_source = op.inputs.first().is_some_and(|i| {
                        op.preconditions.iter().any(|p| {
                            p.subject == format!("source:{}", i.path)
                                && p.state.starts_with("sha256:")
                        })
                    });
                    let hashed_file = op
                        .path
                        .as_deref()
                        .and_then(|rel| adopted_fingerprint(op, rel))
                        .is_some();
                    if !hashed_source || !hashed_file {
                        return Err(plan_invalid(format!(
                            "{} replaces a file but the plan records no hash of the file or of its source",
                            op.id
                        )));
                    }
                }
            }
            Action::CreateFile | Action::UpdateFile => {
                if !generator.supports(op) {
                    return Err(Refusal::new(
                        Code::GeneratorUnavailable,
                        format!(
                            "{}: no generator produces {} content in this build; nothing was changed",
                            op.id, op.adapter
                        ),
                    ));
                }
                if op.output_sha256.is_none() {
                    return Err(plan_invalid(format!(
                        "{} is generated but the plan records no output hash",
                        op.id
                    )));
                }
            }
            Action::Remove | Action::CreateDir => {}
            Action::Retire => {
                return Err(plan_invalid(format!(
                    "{} retires entries of an output that is not a co-owned merge",
                    op.id
                )));
            }
            Action::Install | Action::Upgrade | Action::Start | Action::Restart => {
                unreachable!("handled above")
            }
            Action::None => {
                return Err(plan_invalid(format!("{} is planned with no action", op.id)));
            }
        }
    }
    Ok(())
}

/// The allowances of one kind must equal the planned operations of that kind: an id that names
/// none is a usage error, and every planned one needs its own id. The message lists every
/// missing id as a flag to paste.
pub fn check_allow(
    kind: Approval,
    planned: &[String],
    allow: &BTreeSet<String>,
) -> Result<(), Refusal> {
    if let Some(bad) = allow.iter().find(|a| !planned.contains(a)) {
        return Err(Refusal::new(
            Code::Usage,
            format!(
                "{} {bad} names no {} in this plan",
                kind.flag(),
                kind.noun()
            ),
        ));
    }
    let missing: Vec<String> = planned
        .iter()
        .filter(|id| !allow.contains(*id))
        .map(|id| flag(kind, id))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(Refusal::new(
        kind.code(),
        format!(
            "{} {}(s) need approval. Add: {}",
            missing.len(),
            kind.noun(),
            missing.join(" ")
        ),
    ))
}

/// Checks both kinds. A usage error of either kind wins over a missing approval, so the caller
/// reports a wrong id before it asks for more. Otherwise it returns every missing approval, one
/// refusal per kind, so a preview can list all of them.
pub fn check_allowances(
    gates: &Gates,
    disruptive: &BTreeSet<String>,
    adopt: &BTreeSet<String>,
) -> Result<(), Vec<Refusal>> {
    let results = [
        check_allow(
            Approval::Disruptive,
            gates.planned(Approval::Disruptive),
            disruptive,
        ),
        check_allow(Approval::Adopt, gates.planned(Approval::Adopt), adopt),
    ];
    if let Some(usage) = results
        .iter()
        .filter_map(|r| r.as_ref().err())
        .find(|e| e.code == Code::Usage)
    {
        return Err(vec![usage.clone()]);
    }
    let missing: Vec<Refusal> = results.into_iter().filter_map(Result::err).collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
}

/// Refuses before any write when the journal could not take this run's entries. `journal_len`
/// is the stored size of `journal.jsonl`. `settling` is [`Store::settlement_entries`]: settling
/// earlier work appends one entry each before the plan runs, and the room for them is reserved
/// as well. Journal rotation is not built (DFM-19), so a full journal stops every mutating run.
pub fn check_journal_capacity(
    journal_len: u64,
    settling: usize,
    plan: &Plan,
) -> Result<(), Refusal> {
    let journaled = plan
        .operations
        .iter()
        .filter(|o| {
            o.status == PlanStatus::Planned
                || (o.status == PlanStatus::Current && o.action == Action::Remove)
        })
        .count();
    let need = u64::try_from(journaled.saturating_add(settling))
        .unwrap_or(u64::MAX)
        .saturating_mul(JOURNAL_BYTES_PER_OPERATION);
    if journal_len.saturating_add(need) > dfm_model::state::MAX_JOURNAL_BYTES {
        return Err(Refusal::new(
            Code::LimitExceeded,
            format!(
                "the journal ({journal_len} bytes) cannot take the entries of {journaled} operations and {settling} entries of earlier work to settle without passing {} bytes. Nothing was changed. Journal rotation is not built (DFM-19). Fix: compact the journal by hand (docs/state.md, Recovery by hand, journal at its cap)",
                dfm_model::state::MAX_JOURNAL_BYTES
            ),
        ));
    }
    Ok(())
}

struct Exec<'a, 'r> {
    run: &'a Run<'r>,
    plan: &'a Plan,
    /// Bytes cut off a torn final journal line by this run's repair.
    repaired: usize,
    warnings: Vec<String>,
    /// Journal intents per planner journal id, so an id is never reused after a partial apply.
    attempts: BTreeMap<String, u32>,
    /// Paths whose state is unknown (an interrupted or unconfirmed operation). Nothing more is
    /// done to them in this run.
    unknown_paths: BTreeMap<String, String>,
    /// Resources with an `unknown` entry that has no intent record, so no path is known. Every
    /// operation of the resource is blocked. Resource name to journal id.
    unknown_resources: BTreeMap<String, String>,
    /// The last good clock reading (nanoseconds since the epoch). A clock that fails later in the run reuses
    /// it, so no stamp is ever invented.
    clock: Cell<u64>,
}

/// How one operation ended. `Option<String>` is the journal attempt id, present once the
/// intent entry was written.
enum Ended {
    Updated {
        id: String,
        before: Option<String>,
        after: Option<String>,
        owned: Vec<String>,
        reload: Option<String>,
    },
    Current,
    /// A launcher accepted the request and its completion was not observed within the bound
    /// (docs/lifecycle.md). The journal has a `pending` entry; the run goes on and exits 5.
    Pending(Refusal, String),
    /// A stop request (SIGINT or SIGTERM) arrived before the intent entry. Nothing was written.
    NotStarted(Refusal),
    Failed(Refusal, Option<String>),
    Blocked(Refusal),
    Unknown(Refusal, Option<String>),
}

fn blocked(code: Code, detail: impl Into<String>) -> Ended {
    Ended::Blocked(Refusal::new(code, detail))
}

/// A source that no longer matches the plan blocks the operation. Anything else fails it.
fn ended(g: Gather, rel: &str) -> Ended {
    match g {
        Gather::Stale(d) => blocked(Code::PlanStale, format!("{rel}: {d}; nothing was changed")),
        Gather::Failed(r) => failed(r),
    }
}

fn failed(r: Refusal) -> Ended {
    Ended::Failed(r, None)
}

fn step_name(op: &Operation) -> &'static str {
    match op.action {
        Action::CreateDir => JournalEntry::CREATE_DIR,
        Action::CreateLink => "create-link",
        Action::ReplaceLink => "replace-link",
        Action::CreateFile => "create-file",
        Action::UpdateFile => "update-file",
        Action::Remove => "remove",
        Action::Adopt => "adopt",
        Action::AdoptFile => "adopt-file",
        Action::Retire => "retire",
        Action::Install => "install",
        Action::Upgrade => "upgrade",
        Action::Start => "start",
        Action::Restart => "restart",
        Action::None => "none",
    }
}

/// The live file of a merge, as read under the lock.
struct MergeRead {
    live: Option<Vec<u8>>,
    stamp: Option<crate::fsys::Stamp>,
    /// The permission bits of the live file when it was read.
    mode: Option<u32>,
}

/// What a merge will write. The salted hashes to record are made at the settle step, after the
/// last stop check, because a store from an older build gets its salt created there.
struct MergeDecision {
    merged: Merged,
    /// The salt as it was when the decision was made (`None`: not created yet, or a retire).
    salt: Option<Vec<u8>>,
}

/// Facts read again immediately before the effect.
struct Facts {
    /// The fingerprint the ledger records for this path now.
    ledger_fp: Option<String>,
}

impl Exec<'_, '_> {
    /// The current time as a stamp, or the last good reading when the clock fails.
    fn now(&self) -> String {
        if let Ok(t) = now_ns() {
            self.clock.set(t);
        }
        rfc3339(self.clock.get())
    }

    /// The journal id that holds `abs` (or a descendant or ancestor of it) in an unknown state.
    fn path_unknown(&self, abs: &str) -> Option<&str> {
        self.unknown_paths
            .iter()
            .find(|(u, _)| recover::related(u, abs))
            .map(|(_, id)| id.as_str())
    }

    fn abs(&self, rel: &str) -> Result<String, Refusal> {
        self.run
            .home_path
            .join(rel)
            .to_str()
            .map(str::to_string)
            .ok_or_else(|| plan_invalid("HOME is not UTF-8"))
    }

    fn attempt_id(&mut self, op: &Operation) -> String {
        let n = self.attempts.entry(op.journal_id.clone()).or_insert(0);
        *n += 1;
        format!("{}-{n}", op.journal_id)
    }

    fn journal(
        &self,
        op: &Operation,
        id: &str,
        phase: Phase,
        recovery: Option<Recovery>,
        reason: Option<String>,
    ) -> Result<(), Refusal> {
        self.journal_at(op, id, phase, recovery, reason, self.now())
    }

    fn journal_at(
        &self,
        op: &Operation,
        id: &str,
        phase: Phase,
        recovery: Option<Recovery>,
        reason: Option<String>,
        at: String,
    ) -> Result<(), Refusal> {
        self.run.store.append_journal(&JournalEntry {
            schema: SCHEMA,
            op_id: id.into(),
            op: step_name(op).into(),
            phase,
            at,
            resource: op.resource.clone(),
            adapter: op.adapter.clone(),
            class: op.class,
            recovery,
            token: None,
            reason,
        })
    }

    /// The ledger as the planner sees it, with a path-unsafe entry left out of the view.
    fn owned(&self) -> Result<Vec<Owned>, Refusal> {
        let ledger = self.run.store.read_ledger_lenient()?;
        owned_outputs(&ledger, self.run.home_path)
    }

    /// Re-observes the path and its parents through the opened HOME and compares every recorded
    /// `path:` and `ledger:` precondition with the plan's own string builders. A mismatch blocks
    /// the operation before it writes anything.
    fn prerequisites(&self, op: &Operation, rel: &str) -> Result<Facts, Ended> {
        let sensitive = op.class == Class::Sensitive;
        let hash = op
            .preconditions
            .iter()
            .any(|p| p.subject == format!("path:{rel}") && p.state.contains(":sha256:"));
        let obs = observe(
            &self.run.sys,
            self.run.home,
            &[Request {
                path: rel.into(),
                hash,
                list: false,
            }],
        )
        .map_err(failed)?;
        let owned = self.owned().map_err(failed)?;
        let entry = owned.iter().find(|o| fold::eq(&o.path, rel));
        // Settling an earlier `unknown` just recorded this very link: adoption has nothing left
        // to do, and the operation is not stale. A takeover (`adopt-file`) also changed the path
        // itself, so it is current only while the path is that link.
        let recorded = matches!(op.action, Action::Adopt | Action::AdoptFile)
            && entry.is_some_and(|e| {
                e.resource == op.resource
                    && op.link.as_deref().map(link_fingerprint).as_deref()
                        == e.fingerprint.as_deref()
            })
            && (op.action == Action::Adopt
                || obs.seen.get(rel).is_some_and(|s| {
                    matches!(&s.node, Node::Link { target, .. } if op.link.as_deref() == Some(target.as_str()))
                }));
        for p in &op.preconditions {
            let actual = if let Some(path) = p.subject.strip_prefix("path:") {
                match obs.seen.get(path) {
                    Some(s) => node_state(s, sensitive && path == rel),
                    // The walk stops below a missing or non-directory ancestor.
                    None => "missing".into(),
                }
            } else if p.subject.starts_with("ledger:") {
                ledger_state(entry, sensitive)
            } else {
                continue;
            };
            let settled = p.subject.starts_with("ledger:")
                || (op.action == Action::AdoptFile && p.subject == format!("path:{rel}"));
            if actual != p.state && recorded && settled {
                return Err(Ended::Current);
            }
            if actual != p.state {
                return Err(blocked(
                    Code::PlanStale,
                    format!(
                        "{} was {} and is now {actual}; nothing was changed for it",
                        p.subject, p.state
                    ),
                ));
            }
        }
        Ok(Facts {
            ledger_fp: entry.and_then(|e| e.fingerprint.clone()),
        })
    }

    /// Opens the fragment directory of `op` (if it has one) and reads the sources and fragments
    /// the plan recorded, each compared with the plan. A stale input blocks the operation.
    fn fragment_inputs(
        &self,
        op: &Operation,
        rel: &str,
        salt: Option<&[u8]>,
    ) -> Result<GeneratorInputs, Ended> {
        let dir = match op.fragment_dir.as_deref() {
            Some(d) => {
                let comps = components(d).map_err(failed)?;
                // A fragment directory that does not exist yet holds no fragments.
                self.run
                    .home
                    .open_below_opt(&self.run.sys, &comps)
                    .map_err(Ended::Blocked)?
            }
            None => None,
        };
        gather(self.plan, op, &self.run.sys, dir.as_ref(), salt).map_err(|g| ended(g, rel))
    }

    /// The bytes of a generated file. The inputs are read and compared with the plan first, the
    /// generator runs on those bytes only, and the result must equal the planned output hash.
    /// A stale input or output blocks the operation. Nothing is journaled and nothing is
    /// written, so the previous output stays the last good one.
    fn generated(&self, op: &Operation, rel: &str) -> Result<Vec<u8>, Ended> {
        let inputs = self.fragment_inputs(op, rel, None)?;
        let bytes = match self.run.generator.generate(op, &inputs) {
            Ok(b) => b,
            Err(r) if r.code == Code::PlanStale => return Err(Ended::Blocked(r)),
            Err(r) => return Err(failed(r)),
        };
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FILE {
            return Err(failed(Refusal::new(
                Code::LimitExceeded,
                format!("{rel}: generated content exceeds 16 MiB"),
            )));
        }
        if op.output_sha256.as_deref() != Some(sha256_hex(&bytes).as_str()) {
            return Err(blocked(
                Code::PlanStale,
                format!(
                    "{rel}: generated content differs from the planned output; nothing was changed"
                ),
            ));
        }
        Ok(bytes)
    }

    /// Blocks an operation whose path (or resource, for an entry with no recorded path) holds
    /// an `unknown` outcome that no probe settled.
    fn unknown_block(&self, op: &Operation, rel: &str, abs: &str) -> Result<(), Ended> {
        if let Some(id) = self.path_unknown(abs) {
            return Err(blocked(
                Code::OpInterrupted,
                format!(
                    "{rel}: operation {id} left it in an unknown state. Nothing was changed. {}",
                    recover::fix_text(
                        id,
                        self.unknown_paths.values().filter(|v| *v == id).count() > 1
                    )
                ),
            ));
        }
        if let Some(id) = self.unknown_resources.get(&op.resource) {
            return Err(blocked(
                Code::OpInterrupted,
                format!(
                    "{rel}: resource {} has an unknown outcome (journal id {id}) with no intent record, so no path can be checked. Nothing was changed. Fix: check the machine, then run `dfm state resolve {} --as failed`. Only `--as failed` clears it",
                    op.resource,
                    recover::quoted(id)
                ),
            ));
        }
        Ok(())
    }

    /// The last look at the signal record before an operation writes its intent entry. A signal
    /// that arrived during the checks, the link walk or the generation ends the operation as
    /// not started, with nothing journaled.
    fn stop_requested(&self) -> Result<(), Ended> {
        crate::signal::requested().map_or(Ok(()), |r| Err(Ended::NotStarted(r)))
    }

    fn run_op(&mut self, op: &Operation) -> Ended {
        self.try_op(op).unwrap_or_else(|e| e)
    }

    /// Classifies an error that happened after the intent entry. An unconfirmed durability
    /// result means the effect is in place and only its durability is unknown: the operation
    /// is `unknown`, its path is blocked for the rest of the run, and it is not a plain failure.
    fn after_intent(&mut self, e: Refusal, id: &str, abs: &str) -> Ended {
        if e.code == Code::IoUnconfirmed {
            self.unknown_paths.insert(abs.into(), id.into());
            Ended::Unknown(e, Some(id.into()))
        } else {
            Ended::Failed(e, Some(id.into()))
        }
    }

    /// Journals the failure of an effect after its intent: `failed`, or `unknown` with the
    /// [`recover::UNCONFIRMED`] prefix when only durability is unconfirmed. A journal write
    /// that fails too is named in the refusal. A merge keeps its crash block either way: its
    /// path has no expected hash, so recovery cannot own it.
    fn fail_after_intent(&mut self, op: &Operation, id: &str, abs: &str, e: Refusal) -> Ended {
        let unconfirmed = e.code == Code::IoUnconfirmed;
        // The journal keeps escaped text. The refusal keeps the raw text, which the command
        // escapes once when it prints it.
        let raw: String = e.detail.chars().take(1024).collect();
        let why = dfm_model::display::bounded_visible(&e.detail, 1024);
        let (phase, reason) = if unconfirmed {
            (Phase::Unknown, format!("{}{why}", recover::UNCONFIRMED))
        } else {
            (Phase::Failed, why)
        };
        let e = match self.journal(op, id, phase, None, Some(reason)) {
            Ok(()) => e,
            Err(n) => Refusal::new(
                e.code,
                format!("{raw}; the outcome could not be journaled: {}", n.detail),
            ),
        };
        self.after_intent(e, id, abs)
    }

    fn try_op(&mut self, op: &Operation) -> Result<Ended, Ended> {
        if matches!(op.action, Action::Install | Action::Upgrade) {
            return self.try_install(op);
        }
        if matches!(op.action, Action::Start | Action::Restart) {
            return self.try_service(op);
        }
        // The gates admit one sensitive kind, the co-owned merge, which has its own path. Nothing
        // below hashes, fingerprints or journals the content of a sensitive path.
        if op.class == Class::Sensitive {
            if self.run.generator.merges(op) {
                return self.try_merge(op);
            }
            if self.run.generator.renders(op) {
                return self.try_render(op);
            }
            return Err(failed(Refusal::new(
                Code::SensitiveUnsupported,
                format!("{}: sensitive outputs are not supported", op.id),
            )));
        }
        let rel = op
            .path
            .as_deref()
            .ok_or_else(|| failed(plan_invalid("no path")))?;
        let comps = components(rel).map_err(failed)?;
        let abs = self.abs(rel).map_err(failed)?;
        self.unknown_block(op, rel, &abs)?;
        let facts = self.prerequisites(op, rel)?;
        let (name, parents) = comps
            .split_last()
            .ok_or_else(|| failed(plan_invalid("empty path")))?;
        // The topology check: the parent chain is walked again from HOME without following a
        // link. Everything below acts on this descriptor, not on the path.
        let parent = self
            .run
            .home
            .open_below(&self.run.sys, parents)
            .map_err(Ended::Blocked)?;
        let live = parent.peek(&self.run.sys, name).map_err(failed)?;
        let appeared = |what: &str| blocked(Code::Conflict, format!("{rel} {what}"));
        match (op.action, &live) {
            // The planner blocks a leaf of another user. A leaf swapped in after the plan is
            // caught here, because the prerequisites compare no owner.
            (
                Action::ReplaceLink
                | Action::UpdateFile
                | Action::Remove
                | Action::Adopt
                | Action::AdoptFile,
                Peek::File { own: false, .. }
                | Peek::Link { own: false, .. }
                | Peek::Other { own: false, .. },
            ) => {
                return Err(blocked(
                    Code::PrivilegeUnsupported,
                    format!("{rel} is owned by another user; a HOME adapter never escalates"),
                ));
            }
            (Action::CreateDir | Action::CreateLink | Action::CreateFile, Peek::Missing) => {}
            (Action::CreateDir | Action::CreateLink | Action::CreateFile, _) => {
                return Err(appeared(
                    "appeared before it could be created; it is left alone",
                ));
            }
            (Action::ReplaceLink, Peek::Link { .. }) | (Action::UpdateFile, Peek::File { .. }) => {
                if facts.ledger_fp.is_none() {
                    return Err(appeared("has no ledger fingerprint; it is left alone"));
                }
            }
            (Action::Remove, Peek::File { .. } | Peek::Link { .. }) => {}
            (Action::Adopt, Peek::Link { target, .. })
                if Some(target.as_str()) == op.link.as_deref() => {}
            (Action::AdoptFile, Peek::File { .. }) => {}
            (
                Action::ReplaceLink
                | Action::UpdateFile
                | Action::Remove
                | Action::Adopt
                | Action::AdoptFile,
                _,
            ) => {
                return Err(appeared(
                    "is no longer the entry the plan found; it is left alone",
                ));
            }
            (
                Action::None
                | Action::Retire
                | Action::Install
                | Action::Upgrade
                | Action::Start
                | Action::Restart,
                _,
            ) => {
                return Err(failed(plan_invalid("no action")));
            }
        }
        // A link publishes a target the plan hashed. The source is read again now, so a checkout
        // that changed since the locked re-plan is never published or recorded as owned.
        if op.action.carries_link() {
            link_source(&self.run.sys, self.plan, op).map_err(|g| ended(g, rel))?;
            self.run.fail.hit("apply-after-prereq").map_err(failed)?;
        }
        // Content and fingerprints are decided before the intent is written, so a generator
        // failure leaves no interruption record.
        let bytes = if is_file_action(op.action) {
            Some(self.generated(op, rel)?)
        } else {
            None
        };
        let new_fp = match (&bytes, op.link.as_deref()) {
            (Some(b), _) => Some(file_fingerprint(b)),
            (None, Some(l)) if op.action.carries_link() => Some(link_fingerprint(l)),
            _ => None,
        };
        let expected = match op.action {
            Action::Remove => facts.ledger_fp.clone(),
            _ => new_fp.clone(),
        };
        self.stop_requested()?;
        let id = self.attempt_id(op);
        let recovery = Recovery {
            outputs: vec![RecoveryOutput {
                path: abs.clone(),
                existed_before: !matches!(live, Peek::Missing),
                expected_sha256: expected,
            }],
            snapshot: None,
        };
        let at = self.now();
        self.journal_at(op, &id, Phase::Intent, Some(recovery), None, at.clone())
            .map_err(|e| Ended::Failed(e, Some(id.clone())))?;
        // From here on the effect may have happened.
        let effect = crate::fsys::staged_as(&id, &at, || {
            self.effect(
                op,
                rel,
                &abs,
                &parent,
                name,
                &live,
                facts.ledger_fp.as_deref(),
                bytes.as_deref(),
                new_fp.as_deref(),
            )
        });
        let removed_fp = match effect {
            Ok(fp) => fp,
            Err(e) => return Err(self.fail_after_intent(op, &id, &abs, e)),
        };
        // The effect is read back. Now the outcome entry, then ownership.
        let published = op.action.publishes();
        // An adoption records ownership of an output that was already there. Nothing was
        // published, so no consumer reload state is recorded for it.
        let records = published || op.action == Action::Adopt;
        if let Err(e) =
            self.settle_outcome(op, &id, &abs, records, bytes.is_some(), new_fp.as_deref())
        {
            return Err(self.after_intent(e, &id, &abs));
        }
        Ok(Ended::Updated {
            id,
            before: match op.action {
                Action::Remove => removed_fp,
                _ => facts.ledger_fp.clone(),
            },
            after: new_fp,
            owned: if records { vec![abs] } else { Vec::new() },
            // An adopted file is replaced by a link to the same bytes: a consumer has nothing
            // new to read, and an `allow_adopt` approval records no reload state.
            reload: (published && op.action != Action::AdoptFile)
                .then(|| RELOAD_NOT_ATTEMPTED.to_string()),
        })
    }

    /// A co-owned merge (`claude-settings/v1`, docs/apply.md "Merge"). The live file is read now,
    /// under the writer lock, and the result is made from THAT content. Only a salted hash of the
    /// managed fields is compared and recorded; no hash of the whole file exists anywhere.
    /// Three steps: [`Exec::merge_read`], [`Exec::merge_decide`], [`Exec::merge_settle`].
    fn try_merge(&mut self, op: &Operation) -> Result<Ended, Ended> {
        let rel = op
            .path
            .as_deref()
            .ok_or_else(|| failed(plan_invalid("no path")))?;
        let comps = components(rel).map_err(failed)?;
        let abs = self.abs(rel).map_err(failed)?;
        self.unknown_block(op, rel, &abs)?;
        self.prerequisites(op, rel)?;
        let (name, parents) = comps
            .split_last()
            .ok_or_else(|| failed(plan_invalid("empty path")))?;
        let parent = self
            .run
            .home
            .open_below(&self.run.sys, parents)
            .map_err(Ended::Blocked)?;
        let read = self.merge_read(op, rel, &parent, name)?;
        let decided = self.merge_decide(op, rel, &abs, &read)?;
        self.merge_settle(op, rel, &abs, &parent, name, &read, decided)
    }

    /// Step 1, read: the live file under the lock. Nothing is decided here.
    fn merge_read(
        &self,
        op: &Operation,
        rel: &str,
        parent: &Dir,
        name: &str,
    ) -> Result<MergeRead, Ended> {
        self.run.fail.hit("merge-before-read").map_err(failed)?;
        match (parent.read_live(&self.run.sys, name, MAX_FILE), op.action) {
            (Err(e), _) => Err(Ended::Blocked(e)),
            (Ok(Live::Missing), Action::CreateFile) => Ok(MergeRead {
                live: None,
                stamp: None,
                mode: None,
            }),
            (Ok(Live::Missing), _) => Err(blocked(
                Code::Conflict,
                format!("{rel} is gone since the plan; it is not recreated by this operation"),
            )),
            (Ok(Live::File { .. }), Action::CreateFile) => Err(blocked(
                Code::Conflict,
                format!("{rel} appeared before it could be created; it is left alone"),
            )),
            (Ok(Live::File { bytes, mode, stamp }), _) => {
                // The adapter's own mode: 0600 for every sensitive output except the oMLX
                // model settings, which the application writes with 0644.
                let allowed = dfm_model::merge::mode_for(&op.adapter);
                if mode & 0o077 & !allowed != 0 {
                    return Err(blocked(
                        Code::Conflict,
                        format!(
                            "{rel} is mode {mode:o}, open to group or others beyond {allowed:o}, and can hold credentials. Fix: run chmod {allowed:o} on it; nothing was changed"
                        ),
                    ));
                }
                Ok(MergeRead {
                    live: Some(bytes),
                    stamp: Some(stamp),
                    mode: Some(mode),
                })
            }
            (Ok(Live::Unusable(why)), _) => Err(blocked(
                Code::Conflict,
                format!("{rel} {why}; it is left alone"),
            )),
            (Ok(Live::TooBig), _) => Err(failed(Refusal::new(
                Code::LimitExceeded,
                format!("{rel} exceeds 16 MiB"),
            ))),
        }
    }

    /// Step 2, decide: merge from the bytes just read, then the conflict check that plan time
    /// uses too (`dfm_model::merge::check_foreign`). `retire` touches no managed field and skips
    /// it. A conflict or a lost salt blocks and writes nothing.
    fn merge_decide(
        &self,
        op: &Operation,
        rel: &str,
        abs: &str,
        read: &MergeRead,
    ) -> Result<MergeDecision, Ended> {
        let ledger = self.run.store.read_ledger_lenient().map_err(failed)?;
        let want = fold(abs);
        let recorded: Option<CoOwned> = ledger
            .resources
            .iter()
            .filter(|r| r.resource == op.resource)
            .flat_map(|r| &r.owned_outputs)
            .find(|o| fold(&o.path) == want)
            .and_then(|o| o.coowned.clone());
        // The fragments of a merge are read like those of an assembly: the plan lists them, each
        // is read again below the opened directory and compared with the plan. A retire reads
        // none, because it removes only what the ledger records.
        let fragments = if op.action == Action::Retire {
            GeneratorInputs::default()
        } else {
            let salt = self.run.store.salt().ok().flatten();
            self.fragment_inputs(op, rel, salt.as_deref())?
        };
        let inputs = GeneratorInputs {
            live: read.live.clone(),
            owned_entries: recorded
                .as_ref()
                .map(|c| c.entries.clone())
                .unwrap_or_default(),
            owned_appended: recorded
                .as_ref()
                .map(|c| c.appended.clone())
                .unwrap_or_default(),
            takeover: op.takeover,
            home: self.run.home.shown().to_str().map(String::from),
            listing: fragments.listing,
            fragments: fragments.fragments,
            ..GeneratorInputs::default()
        };
        let merged = match self.run.generator.merge(op, &inputs) {
            Ok(m) => m,
            Err(r) if matches!(r.code, Code::Conflict | Code::PlanStale) => {
                return Err(Ended::Blocked(r));
            }
            Err(r) => return Err(failed(r)),
        };
        let retire = op.action == Action::Retire;
        // A retire removes owned entries by the hash of the declared entry. It reads no managed
        // field, so it needs no salt and a lost or unusable salt does not stop it.
        let salt = if retire {
            None
        } else {
            match self.run.store.salt() {
                Ok(s) => s,
                Err(e) if recorded.is_some() => return Err(Ended::Blocked(e)),
                Err(e) => return Err(failed(e)),
            }
        };
        if !retire {
            check_foreign(
                read.live.is_some(),
                recorded.as_ref(),
                salt.as_deref(),
                &merged,
            )
            .map_err(|e| {
                let e = Refusal::new(e.code, format!("{rel}: {}", e.detail));
                Ended::Blocked(e)
            })?;
        }
        Ok(MergeDecision { merged, salt })
    }

    /// Step 3, settle: intent, publish, read-back, outcome, ledger.
    #[expect(
        clippy::too_many_arguments,
        reason = "one settle needs the facts of the two steps before it"
    )]
    fn merge_settle(
        &mut self,
        op: &Operation,
        rel: &str,
        abs: &str,
        parent: &Dir,
        name: &str,
        read: &MergeRead,
        decided: MergeDecision,
    ) -> Result<Ended, Ended> {
        let MergeDecision { merged, salt } = decided;
        let retire = op.action == Action::Retire;
        self.stop_requested()?;
        // The salt of a store from an older build is created only now, so an operation that
        // ends as not started leaves the state store as it was.
        let (new_hash, leaves) = if retire {
            (None, Default::default())
        } else {
            let salt = match salt {
                Some(s) => s,
                None => self.run.store.salt_or_create().map_err(failed)?,
            };
            (
                Some(salted(&salt, &merged.rendered_view)),
                leaf_hashes(&salt, &merged),
            )
        };
        let id = self.attempt_id(op);
        let recovery = Recovery {
            outputs: vec![RecoveryOutput {
                path: abs.to_string(),
                existed_before: read.live.is_some(),
                expected_sha256: None,
            }],
            snapshot: None,
        };
        let at = self.now();
        self.journal_at(op, &id, Phase::Intent, Some(recovery), None, at.clone())
            .map_err(|e| Ended::Failed(e, Some(id.clone())))?;
        let effect = crate::fsys::staged_as(&id, &at, || {
            self.merge_effect(
                rel,
                parent,
                name,
                read,
                &merged.bytes,
                merged.changed,
                dfm_model::merge::mode_for(&op.adapter),
            )
        });
        if let Err(e) = effect {
            return Err(self.fail_after_intent(op, &id, abs, e));
        }
        let settled = self
            .journal(op, &id, Phase::Done, None, None)
            .and_then(|()| self.run.fail.hit("apply-after-done"))
            .and_then(|()| match &new_hash {
                None => self.forget_output(op, &id, abs),
                Some(h) => self.record_coowned(
                    op,
                    &id,
                    abs,
                    CoOwned {
                        managed_hash: h.clone(),
                        entries: merged.entries.clone(),
                        appended: merged.appended.clone(),
                        leaves,
                    },
                    dfm_model::merge::mode_for(&op.adapter),
                ),
            });
        if let Err(e) = settled {
            return Err(self.after_intent(e, &id, abs));
        }
        Ok(Ended::Updated {
            id,
            before: None,
            after: None,
            owned: if retire {
                Vec::new()
            } else {
                vec![abs.to_string()]
            },
            reload: merged.changed.then(|| RELOAD_NOT_ATTEMPTED.to_string()),
        })
    }

    /// Publishes the merged (or rendered) bytes when they differ from the live file, then reads
    /// the result back. The replace is guarded by the identity captured at the re-read, not by a
    /// hash of the whole file. The read-back always runs, also when nothing was written: the
    /// bytes must be the bytes in memory, and the mode must be the mode this operation left
    /// (`mode` after a write, the mode read before for an unchanged file). It compares in memory
    /// and records nothing.
    #[expect(
        clippy::too_many_arguments,
        reason = "one effect needs the facts read before it"
    )]
    fn merge_effect(
        &self,
        rel: &str,
        parent: &Dir,
        name: &str,
        read: &MergeRead,
        bytes: &[u8],
        changed: bool,
        mode: u32,
    ) -> Result<(), Refusal> {
        let fail = self.run.fail;
        fail.hit("apply-before-effect")?;
        if changed {
            let how = match read.stamp {
                Some(s) => Publish::ReplaceStamp(s),
                None => Publish::Create,
            };
            parent.publish_file(name, bytes, mode, how, MAX_FILE, fail)?;
        }
        fail.hit("apply-after-effect")?;
        let bad = |what: &str| {
            Refusal::new(
                Code::IoFailed,
                format!("postcondition failed for {rel}: {what}"),
            )
        };
        let (h, _) = parent
            .hash_file(name, MAX_FILE)?
            .ok_or_else(|| bad("the file cannot be read back"))?;
        if h != file_fingerprint(bytes) {
            return Err(bad("the content read back differs"));
        }
        let want_mode = if changed { Some(mode) } else { read.mode };
        if parent.entry_mode(name)? != want_mode {
            return Err(bad("the mode differs"));
        }
        Ok(())
    }

    /// A rendered secret (`op-template/v1`, docs/secrets.md). Like a merge, the live file is read
    /// under the writer lock and compared by a per-machine salted hash only; nothing records or
    /// prints an unsalted hash or a byte of the content. Order: prerequisites, re-read, ledger
    /// check (a file the ledger does not prove ours needs the `takeover` approval), the template
    /// read from the source root, the render, then intent, publish, read-back, outcome, ledger.
    /// A render that fails or is empty fails the operation before the intent entry, so the
    /// previous file stays the last good one. Rendered bytes equal to the live file with a
    /// matching ledger hash are `current`: nothing is journaled or written.
    fn try_render(&mut self, op: &Operation) -> Result<Ended, Ended> {
        let rel = op
            .path
            .as_deref()
            .ok_or_else(|| failed(plan_invalid("no path")))?;
        let comps = components(rel).map_err(failed)?;
        let abs = self.abs(rel).map_err(failed)?;
        self.unknown_block(op, rel, &abs)?;
        self.prerequisites(op, rel)?;
        let (name, parents) = comps
            .split_last()
            .ok_or_else(|| failed(plan_invalid("empty path")))?;
        let parent = self
            .run
            .home
            .open_below(&self.run.sys, parents)
            .map_err(Ended::Blocked)?;
        let read = self.merge_read(op, rel, &parent, name)?;
        // What the ledger records for this path, if the manager wrote it before.
        let ledger = self.run.store.read_ledger_lenient().map_err(failed)?;
        let want = fold(&abs);
        let recorded: Option<String> = ledger
            .resources
            .iter()
            .filter(|r| r.resource == op.resource)
            .flat_map(|r| &r.owned_outputs)
            .find(|o| fold(&o.path) == want)
            .and_then(|o| o.coowned.as_ref().map(|c| c.managed_hash.clone()));
        let salt = self.run.store.salt().map_err(Ended::Blocked)?;
        let salted_live = match (&read.live, &salt) {
            (Some(live), Some(s)) => Some(salted(s, live)),
            _ => None,
        };
        // A file the plan took for ours must still be ours now. A takeover was approved by id.
        if read.live.is_some() && !op.takeover && (recorded.is_none() || salted_live != recorded) {
            return Err(blocked(
                Code::Conflict,
                format!(
                    "{rel} changed since the plan and the ledger no longer proves the manager wrote it; nothing was changed. Fix: plan again and approve the takeover with --allow-adopt"
                ),
            ));
        }
        self.run.fail.hit("apply-after-prereq").map_err(failed)?;
        let template = self.template(op)?;
        // A render that fails is a failed operation (E_RENDER_FAILED), also when the cause is a
        // session that went away after the gate: the provider does not say which it was. A
        // template that is not the one the plan was resolved from blocks it (E_PLAN_STALE).
        let bytes = self
            .run
            .generator
            .render(op, &template)
            .map_err(|e| match e.code {
                Code::PlanStale => Ended::Blocked(e),
                _ => failed(e),
            })?;
        if bytes.is_empty() {
            return Err(failed(Refusal::new(
                Code::RenderFailed,
                format!("{rel}: the provider rendered nothing; the previous file is kept"),
            )));
        }
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FILE {
            return Err(failed(Refusal::new(
                Code::LimitExceeded,
                format!("{rel}: rendered content exceeds 16 MiB"),
            )));
        }
        let mode = op.mode.unwrap_or(0o600);
        let changed = read.live.as_deref() != Some(bytes.as_slice()) || read.mode != Some(mode);
        if !changed && recorded.is_some() && salted_live == recorded {
            return Ok(Ended::Current);
        }
        self.stop_requested()?;
        let salt = match salt {
            Some(s) => s,
            None => self.run.store.salt_or_create().map_err(failed)?,
        };
        let new_hash = salted(&salt, &bytes);
        let id = self.attempt_id(op);
        let recovery = Recovery {
            outputs: vec![RecoveryOutput {
                path: abs.clone(),
                existed_before: read.live.is_some(),
                expected_sha256: None,
            }],
            snapshot: None,
        };
        let at = self.now();
        self.journal_at(op, &id, Phase::Intent, Some(recovery), None, at.clone())
            .map_err(|e| Ended::Failed(e, Some(id.clone())))?;
        let effect = crate::fsys::staged_as(&id, &at, || {
            self.merge_effect(rel, &parent, name, &read, &bytes, changed, mode)
        });
        if let Err(e) = effect {
            return Err(self.fail_after_intent(op, &id, &abs, e));
        }
        let settled = self
            .journal(op, &id, Phase::Done, None, None)
            .and_then(|()| self.run.fail.hit("apply-after-done"))
            .and_then(|()| {
                self.record_coowned(
                    op,
                    &id,
                    &abs,
                    CoOwned {
                        managed_hash: new_hash,
                        entries: Vec::new(),
                        appended: Vec::new(),
                        leaves: Default::default(),
                    },
                    mode,
                )
            });
        if let Err(e) = settled {
            return Err(self.after_intent(e, &id, &abs));
        }
        Ok(Ended::Updated {
            id,
            before: None,
            after: None,
            owned: vec![abs],
            reload: changed.then(|| RELOAD_NOT_ATTEMPTED.to_string()),
        })
    }

    /// An unresolved install blocks a package even when the provider now lists it as installed.
    fn unknown_install(&self, op: &Operation) -> Result<(), Ended> {
        if let Some(id) = self.unknown_resources.get(&op.resource) {
            return Err(blocked(
                Code::OpInterrupted,
                format!(
                    "{}: resource {} has an unknown outcome (journal id {id}) with no recorded output path. Nothing was changed. Fix: check the provider's state, then run `dfm state resolve {} --as failed`",
                    op.id,
                    op.resource,
                    recover::quoted(id)
                ),
            ));
        }
        Ok(())
    }

    /// A package install (docs/packages.md) or upgrade (docs/toolchains.md). The provider is
    /// probed again under the lock: a package that is installed now is `current`, any other
    /// change blocks. Then the intent entry (no path: the provider's listing is the record), the
    /// install or upgrade, a third probe as the postcondition, and the outcome entry. For an
    /// upgrade the listing before and after decides: unchanged is `current`, changed is
    /// `updated`. No ledger entry is written.
    fn try_install(&mut self, op: &Operation) -> Result<Ended, Ended> {
        self.unknown_install(op)?;
        let want = op
            .preconditions
            .iter()
            .find(|p| p.subject.starts_with("probe:"))
            .map(|p| p.state.as_str())
            .ok_or_else(|| failed(plan_invalid("an install needs a probe precondition")))?;
        let now = self.run.generator.probe(op).map_err(failed)?;
        // A stop request that ended the probe is the stop, not a probe failure: nothing started.
        self.stop_requested()?;
        match &now {
            PackageState::Installed(_) => return Ok(Ended::Current),
            s if s.still_planned(want) => {}
            // The provider could not answer now: that is the probe's failure, not a stale plan.
            PackageState::Unknown(why) => {
                return Err(blocked(
                    Code::ProbeFailed,
                    format!(
                        "{}: the provider could not report the installed state now ({why}); nothing was changed",
                        op.id
                    ),
                ));
            }
            s => {
                return Err(blocked(
                    Code::PlanStale,
                    format!(
                        "{}: the provider reported {want} at plan time and {} now; nothing was changed",
                        op.id,
                        s.state()
                    ),
                ));
            }
        }
        self.stop_requested()?;
        let before = now.state();
        let id = self.attempt_id(op);
        let recovery = Recovery {
            outputs: Vec::new(),
            snapshot: None,
        };
        self.journal(op, &id, Phase::Intent, Some(recovery), None)
            .map_err(|e| Ended::Failed(e, Some(id.clone())))?;
        // From here on the provider may have changed the machine. A failure is journaled.
        let result = self
            .run
            .fail
            .hit("apply-before-effect")
            .and_then(|()| self.run.generator.install(op))
            .and_then(|()| self.run.fail.hit("apply-after-effect"))
            .and_then(|()| {
                let now = self.run.generator.probe(op)?;
                match (now, crate::signal::requested()) {
                    (now @ (PackageState::Installed(_) | PackageState::Rolling(_)), _) => {
                        Ok(now.state())
                    }
                    // The stop ended the check itself, so the install's outcome is unknown, not
                    // failed. The next run probes again.
                    (_, Some(stop)) => Err(Refusal::new(
                        stop.code,
                        format!(
                            "{}: stopped before the postcondition was checked; the install may have completed, and the next run probes again",
                            op.id
                        ),
                    )),
                    (s, None) => Err(Refusal::new(
                        Code::IoFailed,
                        format!(
                            "postcondition failed for {}: the provider reports {} after the install",
                            op.id,
                            s.state()
                        ),
                    )),
                }
            });
        let after = match result {
            Ok(after) => after,
            Err(e) => {
                let why = dfm_model::display::bounded_visible(&e.detail, 1024);
                let e = match self.journal(op, &id, Phase::Failed, None, Some(why)) {
                    Ok(()) => e,
                    Err(n) => Refusal::new(
                        e.code,
                        format!(
                            "{}; the outcome could not be journaled: {}",
                            e.detail, n.detail
                        ),
                    ),
                };
                return Err(Ended::Failed(e, Some(id)));
            }
        };
        if let Err(e) = self.journal(op, &id, Phase::Done, None, None) {
            return Err(Ended::Failed(e, Some(id)));
        }
        // An upgrade whose provider already had its newest version changed nothing: the
        // listing is the same, so the operation is current, with the attempt journaled.
        if op.action == Action::Upgrade && after == before {
            return Ok(Ended::Current);
        }
        Ok(Ended::Updated {
            id,
            before: None,
            after: None,
            owned: Vec::new(),
            reload: None,
        })
    }

    /// The declared prerequisite files of a service, observed again under the lock and compared
    /// with the plan: the `path:` preconditions (the entry as the planner saw it) and the
    /// `content:` precondition (the bytes of the definition, read through a link, as the adapter
    /// hashed them at plan time). A file that changed or went away since the plan blocks the
    /// start or restart, so nothing loads a definition the reviewer did not see.
    fn service_prerequisites(&self, op: &Operation) -> Result<(), Ended> {
        for p in &op.preconditions {
            let Some(path) = p.subject.strip_prefix("content:") else {
                continue;
            };
            let actual = match definition_bytes(&self.run.home_path.join(path)) {
                Ok(b) => format!("sha256:{}", sha256_hex(&b)),
                Err(why) => why,
            };
            if actual != p.state {
                return Err(blocked(
                    Code::PlanStale,
                    format!(
                        "{} was {} and is now {actual}; nothing was loaded",
                        p.subject, p.state
                    ),
                ));
            }
        }
        // The planner recorded a sensitive prerequisite (`sensitive-path:`) by mode only, with
        // no hash: it is observed and compared the same way, so no hash of it is computed or
        // printed here either. The subject carries the decision; the state's shape does not.
        fn kind(p: &dfm_model::plan::Precondition) -> Option<(&str, bool)> {
            if let Some(path) = p.subject.strip_prefix("path:") {
                Some((path, false))
            } else {
                p.subject
                    .strip_prefix("sensitive-path:")
                    .map(|path| (path, true))
            }
        }
        let reqs: Vec<Request> = op
            .preconditions
            .iter()
            .filter_map(kind)
            .map(|(path, sensitive)| Request {
                path: path.to_string(),
                hash: !sensitive,
                list: false,
            })
            .collect();
        if reqs.is_empty() {
            return Ok(());
        }
        let obs = observe(&self.run.sys, self.run.home, &reqs).map_err(failed)?;
        for p in &op.preconditions {
            let Some((path, sensitive)) = kind(p) else {
                continue;
            };
            let actual = obs
                .seen
                .get(path)
                .map_or_else(|| "missing".to_string(), |s| node_state(s, sensitive));
            if actual != p.state {
                return Err(blocked(
                    Code::PlanStale,
                    format!(
                        "{} was {} and is now {actual}; nothing was changed for it",
                        p.subject, p.state
                    ),
                ));
            }
        }
        Ok(())
    }

    /// A service start or restart (docs/lifecycle.md). The prerequisite files and the launcher
    /// are checked again under the lock: a service that is as declared now is `current`, any
    /// other change blocks. Then the intent entry (no path: the launcher's report is the record),
    /// the action with its bounded completion wait, a second report as the postcondition, and
    /// the outcome entry: `done` with `reloaded`, `pending` with `requested` when the launcher
    /// accepted the request without reporting completion, or `failed`. No ledger entry is written.
    fn try_service(&mut self, op: &Operation) -> Result<Ended, Ended> {
        self.unknown_install(op)?;
        self.service_prerequisites(op)?;
        let want = op
            .preconditions
            .iter()
            .find(|p| p.subject.starts_with("probe:"))
            .map(|p| p.state.as_str())
            .ok_or_else(|| failed(plan_invalid("a service needs a probe precondition")))?;
        let now = self.run.generator.service_state(op).map_err(failed)?;
        self.stop_requested()?;
        match &now {
            s if s.state() == want => {}
            // Someone started it meanwhile: the state matches what a start would make.
            ServiceState::Current(_) if op.action == Action::Start => return Ok(Ended::Current),
            ServiceState::Unknown(why) => {
                return Err(blocked(
                    Code::ProbeFailed,
                    format!(
                        "{}: the launcher could not report the service state now ({why}); nothing was changed",
                        op.id
                    ),
                ));
            }
            s => {
                return Err(blocked(
                    Code::PlanStale,
                    format!(
                        "{}: the launcher reported {want} at plan time and {} now; nothing was changed",
                        op.id,
                        s.state()
                    ),
                ));
            }
        }
        self.stop_requested()?;
        let id = self.attempt_id(op);
        let recovery = Recovery {
            outputs: Vec::new(),
            snapshot: None,
        };
        self.journal(op, &id, Phase::Intent, Some(recovery), None)
            .map_err(|e| Ended::Failed(e, Some(id.clone())))?;
        // From here on the launcher may have changed the machine. Every outcome is journaled.
        let result = self
            .run
            .fail
            .hit("apply-before-effect")
            .and_then(|()| self.run.generator.service_act(op))
            .and_then(|c| self.run.fail.hit("apply-after-effect").map(|()| c))
            .and_then(|completion| match completion {
                Completion::Queued(detail) => Ok(Some(detail)),
                Completion::Completed => {
                    let now = self.run.generator.service_state(op)?;
                    match (now, crate::signal::requested()) {
                        (ServiceState::Current(_), _) => Ok(None),
                        // The stop ended the check itself: the outcome is unknown, not failed.
                        (_, Some(stop)) => Err(Refusal::new(
                            stop.code,
                            format!(
                                "{}: stopped before the postcondition was checked; the request may have completed, and the next run probes again",
                                op.id
                            ),
                        )),
                        (s, None) => Err(Refusal::new(
                            Code::IoFailed,
                            format!(
                                "postcondition failed for {}: the launcher reports {} after the {}",
                                op.id,
                                s.state(),
                                step_name(op)
                            ),
                        )),
                    }
                }
            });
        let queued = match result {
            Ok(q) => q,
            Err(e) => {
                let why = dfm_model::display::bounded_visible(&e.detail, 1024);
                let e = match self.journal(op, &id, Phase::Failed, None, Some(why)) {
                    Ok(()) => e,
                    Err(n) => Refusal::new(
                        e.code,
                        format!(
                            "{}; the outcome could not be journaled: {}",
                            e.detail, n.detail
                        ),
                    ),
                };
                return Err(Ended::Failed(e, Some(id)));
            }
        };
        if let Some(detail) = queued {
            let r = Refusal::new(
                Code::Pending,
                format!(
                    "{}: the launcher accepted the {} and did not report completion in time ({detail}); the next run probes again",
                    op.id,
                    step_name(op)
                ),
            );
            let why = dfm_model::display::bounded_visible(&r.detail, 1024);
            if let Err(e) = self.journal(op, &id, Phase::Pending, None, Some(why)) {
                return Err(Ended::Failed(e, Some(id)));
            }
            return Ok(Ended::Pending(r, id));
        }
        if let Err(e) = self.journal(op, &id, Phase::Done, None, None) {
            return Err(Ended::Failed(e, Some(id)));
        }
        Ok(Ended::Updated {
            id,
            before: None,
            after: None,
            owned: Vec::new(),
            reload: Some(RELOAD_RELOADED.to_string()),
        })
    }

    /// The template of a secret operation, read from the plan's source root as apply reads any
    /// source: below the root without following a link, a regular file of the user with one
    /// hard link, within the source file bound. The resolver records no content identity for
    /// it, so presence and shape are what is checked here (docs/apply.md, "Link source"); the
    /// generator then compares the bytes with the sources the plan was resolved from.
    fn template(&self, op: &Operation) -> Result<Vec<u8>, Ended> {
        let [input] = op.inputs.as_slice() else {
            return Err(failed(plan_invalid("a secret needs exactly one template")));
        };
        let [source] = self.plan.sources.as_slice() else {
            return Err(failed(plan_invalid(
                "a plan with a secret must have exactly one source",
            )));
        };
        let root = Dir::open_plain(Path::new(&source.root)).map_err(failed)?;
        match root
            .read_input_below(
                &self.run.sys,
                &input.path,
                dfm_model::plan::MAX_SOURCE_FILE_BYTES,
            )
            .map_err(failed)?
        {
            crate::fsys::Got::Bytes(b) => Ok(b),
            crate::fsys::Got::Absent => Err(blocked(
                Code::PlanStale,
                format!(
                    "template {} changed since the plan: it is gone, not a plain file of yours or linked twice; nothing was changed",
                    input.path
                ),
            )),
            crate::fsys::Got::TooBig => Err(failed(Refusal::new(
                Code::LimitExceeded,
                format!("template {} exceeds 1 MiB", input.path),
            ))),
        }
    }

    /// Records the co-owned file: no content fingerprint, only the salted managed hash and the
    /// entries the manager added.
    fn record_coowned(
        &self,
        op: &Operation,
        id: &str,
        abs: &str,
        coowned: CoOwned,
        mode: u32,
    ) -> Result<(), Refusal> {
        let out = OwnedOutput {
            path: abs.into(),
            mode,
            fingerprint: None,
            coowned: Some(coowned),
        };
        let who = (op.resource.as_str(), op.adapter.as_str(), op.class);
        recover::record_output(self.run.store, who, id, out)
    }

    /// The outcome entry and ownership. A create or remove changes the ledger after `done`. An
    /// update or replace already has a ledger entry with the old fingerprint, so the ledger
    /// moves first: a crash or a failed write then leaves an open intent, and recovery repairs
    /// the fingerprint. The other order would leave `done` with a stale fingerprint.
    fn settle_outcome(
        &self,
        op: &Operation,
        id: &str,
        abs: &str,
        records: bool,
        generated: bool,
        new_fp: Option<&str>,
    ) -> Result<(), Refusal> {
        let own = || match op.action {
            Action::Remove => self.forget_output(op, id, abs),
            _ if records => {
                let mode = if generated {
                    op.mode.unwrap_or(0o600)
                } else {
                    0o777
                };
                self.record_output(op, id, abs, new_fp.unwrap_or_default(), mode)
            }
            _ => Ok(()),
        };
        // An adopted file moves the ledger first too: an interruption then leaves an open intent
        // that settling closes with ownership, not a link no entry records.
        let ledger_first = matches!(
            op.action,
            Action::UpdateFile | Action::ReplaceLink | Action::AdoptFile
        );
        if ledger_first {
            own()?;
        }
        self.journal(op, id, Phase::Done, None, None)?;
        self.run.fail.hit("apply-after-done")?;
        if ledger_first { Ok(()) } else { own() }
    }

    /// The effect, then its read-back. For a removal it returns the fingerprint of what was
    /// removed. The primitives re-verify the leaf themselves (create never replaces, replace
    /// and update compare the live link or content against the ledger fingerprint, remove
    /// checks identity).
    #[expect(
        clippy::too_many_arguments,
        reason = "one effect needs the facts verified before it"
    )]
    fn effect(
        &self,
        op: &Operation,
        rel: &str,
        abs: &str,
        parent: &Dir,
        name: &str,
        live: &Peek,
        ledger_fp: Option<&str>,
        bytes: Option<&[u8]>,
        new_fp: Option<&str>,
    ) -> Result<Option<String>, Refusal> {
        let sys = &self.run.sys;
        let fail = self.run.fail;
        fail.hit("apply-before-effect")?;
        let mut removed = None;
        match op.action {
            Action::CreateDir => parent.make_dir(name, fail)?,
            Action::CreateLink => {
                parent.make_link(name, op.link.as_deref().unwrap_or_default(), fail)?;
            }
            Action::ReplaceLink => parent.replace_link(
                name,
                op.link.as_deref().unwrap_or_default(),
                ledger_fp.unwrap_or_default(),
                sys.uid,
                fail,
            )?,
            Action::AdoptFile => parent.replace_file_with_link(
                name,
                op.link.as_deref().unwrap_or_default(),
                adopted_fingerprint(op, rel).unwrap_or_default(),
                sys.uid,
                // The source was checked before the intent. A checkout since then must not
                // leave a link to other bytes where the only copy of the file was.
                || {
                    link_source(sys, self.plan, op).map_err(|g| match g {
                        Gather::Stale(d) => Refusal::new(
                            Code::Conflict,
                            format!("{rel}: {d}; the file is left alone"),
                        ),
                        Gather::Failed(r) => r,
                    })
                },
                fail,
            )?,
            Action::CreateFile | Action::UpdateFile => {
                let how = match ledger_fp {
                    Some(h) if op.action == Action::UpdateFile => Publish::Replace {
                        expected_sha256: h,
                        uid: sys.uid,
                    },
                    _ => Publish::Create,
                };
                parent.publish_file(
                    name,
                    bytes.unwrap_or_default(),
                    op.mode.unwrap_or(0o600),
                    how,
                    MAX_FILE,
                    fail,
                )?;
            }
            Action::Remove => removed = Some(self.remove(op, abs, parent, name, live)?),
            // Nothing is written. The read-back below is the last look at the leaf.
            Action::Adopt => {}
            Action::None
            | Action::Retire
            | Action::Install
            | Action::Upgrade
            | Action::Start
            | Action::Restart => {
                return Err(plan_invalid("no action"));
            }
        }
        fail.hit("apply-after-effect")?;
        let bad = |what: &str| {
            Refusal::new(
                Code::IoFailed,
                format!("postcondition failed for {rel}: {what}"),
            )
        };
        match (op.action, parent.peek(sys, name)?) {
            (Action::Adopt, _) => self.still_exact_link(op, abs, parent, name)?,
            (Action::CreateDir, Peek::Dir { .. }) | (Action::Remove, Peek::Missing) => {}
            (
                Action::CreateLink | Action::ReplaceLink | Action::AdoptFile,
                Peek::Link { target, .. },
            ) if Some(target.as_str()) == op.link.as_deref() => {}
            (Action::CreateFile | Action::UpdateFile, Peek::File { .. }) => {
                let (h, _) = parent
                    .hash_file(name, MAX_FILE)?
                    .ok_or_else(|| bad("the file cannot be read back"))?;
                if Some(h.as_str()) != new_fp {
                    return Err(bad("the content read back differs"));
                }
                if parent.entry_mode(name)? != Some(op.mode.unwrap_or(0o600) & 0o777) {
                    return Err(bad("the mode differs"));
                }
            }
            _ => return Err(bad("the entry is not what was published")),
        }
        Ok(removed)
    }

    /// The adoption read-back, immediately before the outcome entry and the ledger write: the
    /// leaf is still a link with the planned text.
    /// A change is a conflict. The entry is left alone and no ledger entry is written.
    fn still_exact_link(
        &self,
        op: &Operation,
        abs: &str,
        parent: &Dir,
        name: &str,
    ) -> Result<(), Refusal> {
        if parent
            .peek(&self.run.sys, name)?
            .is_owned_link_to(op.link.as_deref())
        {
            return Ok(());
        }
        Err(Refusal::new(
            Code::Conflict,
            format!("{abs} is no longer the link the plan found; it is left alone and not adopted"),
        ))
    }

    /// Deletes one owned output. Authorization is decided again here, with the ledger read now
    /// and the live fingerprint taken from a descriptor opened without following a link.
    fn remove(
        &self,
        op: &Operation,
        abs: &str,
        parent: &Dir,
        name: &str,
        live: &Peek,
    ) -> Result<String, Refusal> {
        let conflict =
            |what: &str| Refusal::new(Code::Conflict, format!("{abs} {what}; it is kept"));
        let (fp, ident, target) = match live {
            Peek::File { .. } => {
                let (h, i) = parent
                    .hash_file(name, MAX_FILE)?
                    .ok_or_else(|| conflict("is no longer a readable regular file"))?;
                (h, Some(i), None)
            }
            Peek::Link { target, .. } => (link_fingerprint(target), None, Some(target.clone())),
            _ => return Err(conflict("is not a file or a link")),
        };
        if !self.run.store.authorizes_delete(&op.resource, abs, &fp)? {
            return Err(conflict(
                "no longer matches the fingerprint the ledger records",
            ));
        }
        self.run.fail.hit("apply-before-unlink")?;
        // The name must still be the entry that was hashed.
        match (ident, target) {
            (Some(i), _) if parent.entry_ident(name)? != Some(i) => {
                return Err(conflict("was replaced after it was checked"));
            }
            (None, Some(t)) => match parent.peek(&self.run.sys, name)? {
                Peek::Link { target, .. } if target == t => {}
                _ => return Err(conflict("was replaced after it was checked")),
            },
            _ => {}
        }
        // The identity above names the hashed inode. Its owner is checked beside the unlink.
        parent.remove_owned(name, self.run.sys.uid, self.run.fail)?;
        Ok(fp)
    }

    fn record_output(
        &self,
        op: &Operation,
        id: &str,
        abs: &str,
        fp: &str,
        mode: u32,
    ) -> Result<(), Refusal> {
        let who = (op.resource.as_str(), op.adapter.as_str(), op.class);
        let out = OwnedOutput {
            path: abs.into(),
            mode,
            fingerprint: Some(fp.into()),
            coowned: None,
        };
        recover::record_output(self.run.store, who, id, out)
    }

    /// Drops one output from the ledger, and the resource when it owns nothing else.
    fn forget_output(&self, op: &Operation, id: &str, abs: &str) -> Result<(), Refusal> {
        recover::forget_output(self.run.store, &op.resource, id, abs)
    }

    /// Whether the entry at `rel` is absent right now, read through the opened HOME without
    /// following a link. A parent that cannot be opened, or any doubt, is "not known absent".
    fn absent_now(&self, rel: &str) -> bool {
        let Ok(comps) = components(rel) else {
            return false;
        };
        let Some((name, parents)) = comps.split_last() else {
            return false;
        };
        self.run
            .home
            .open_below(&self.run.sys, parents)
            .and_then(|p| p.peek(&self.run.sys, name))
            .is_ok_and(|live| matches!(live, Peek::Missing))
    }

    /// A removal candidate that is already gone: its ledger entry is stale authority. A later
    /// file with the same content at that path must not look owned, so the entry is dropped.
    /// Absence is checked again right before the ledger write. A path that exists again, or
    /// whose parent cannot be checked, or that is in an unknown state, keeps its entry.
    fn prune(&mut self, op: &Operation) -> Ended {
        let Some(rel) = op.path.as_deref() else {
            return Ended::Current;
        };
        let Ok(abs) = self.abs(rel) else {
            return Ended::Current;
        };
        if self.unknown_resources.contains_key(&op.resource) {
            return Ended::Current;
        }
        let want = fold(&abs);
        let listed = self.run.store.read_ledger_lenient().map(|l| {
            l.resources.iter().any(|r| {
                r.resource == op.resource && r.owned_outputs.iter().any(|o| fold(&o.path) == want)
            })
        });
        match listed {
            Ok(true) if self.path_unknown(&abs).is_none() && self.absent_now(rel) => {}
            Ok(_) => return Ended::Current,
            Err(e) => return failed(e),
        }
        if let Err(e) = self.run.fail.hit("prune-before-intent") {
            return failed(e);
        }
        // A stop request keeps the entry: pruning is only housekeeping, so no intent is written.
        if crate::signal::requested().is_some() {
            return Ended::Current;
        }
        let id = self.attempt_id(op);
        let recovery = Recovery {
            outputs: vec![RecoveryOutput {
                path: abs.clone(),
                existed_before: false,
                expected_sha256: None,
            }],
            snapshot: None,
        };
        if let Err(e) = self.journal(op, &id, Phase::Intent, Some(recovery), None) {
            return Ended::Failed(e, Some(id));
        }
        let r = if self.absent_now(rel) {
            self.forget_output(op, &id, &abs)
                .and_then(|()| self.journal(op, &id, Phase::Done, None, None))
        } else {
            self.journal(
                op,
                &id,
                Phase::Done,
                None,
                Some("kept: the path exists again".into()),
            )
        };
        match r {
            Ok(()) => Ended::Current,
            Err(e) => self.after_intent(e, &id, &abs),
        }
    }
}

/// Journal entries of earlier runs are settled first (docs/apply.md, "Interrupted runs"; DFM-19).
///
/// 1. An `intent` with no terminal entry is probed with the operation's `already_done` check
///    and journaled `done`, `failed` or `unknown`. Nothing is run again and no ownership is
///    created.
/// 2. Every `unknown` entry still outstanding, from this step or from an earlier run, is probed
///    again. A clear answer journals `done` (with ownership as the operation would have
///    recorded it) or `failed`. An unclear one blocks its path in this run.
fn settle(ex: &mut Exec<'_, '_>) -> Result<(), Refusal> {
    // A torn final journal line is cut and journaled before anything reads the entries.
    ex.repaired = ex.run.store.repair_torn_journal()?;
    let entries = ex.run.store.read_journal_lenient()?;
    let settled = {
        let planned = |resource: &str, abs: &str| {
            ex.plan.operations.iter().any(|o| {
                o.resource == resource
                    && o.path
                        .as_deref()
                        .and_then(|p| ex.abs(p).ok())
                        .is_some_and(|a| fold::eq(&a, abs))
            })
        };
        let w = (&ex.run.sys, ex.run.home, ex.run.home_path);
        recover::settle_all(ex.run.store, w, &planned)?
    };
    ex.warnings = settled.warnings;
    for o in settled.open {
        if o.has_paths {
            for p in o.paths {
                ex.unknown_paths.insert(p, o.id.clone());
            }
        } else {
            ex.unknown_resources.insert(o.resource, o.id);
        }
    }
    // Settling appends only terminal entries, so the intents counted here are all there are.
    for entry in entries.iter().filter(|e| e.phase == Phase::Intent) {
        if let Some((jid, _)) = entry.op_id.rsplit_once('-') {
            *ex.attempts.entry(jid.to_string()).or_insert(0) += 1;
        }
    }
    Ok(())
}

fn record(
    op: &Operation,
    id: &str,
    status: Status,
    times: Option<(String, String)>,
    detail: Option<&Refusal>,
    updated: Option<&Ended>,
) -> OperationRecord {
    let fp = |f: &Option<String>| {
        (op.class == Class::Normal)
            .then(|| f.clone().map(|fingerprint| Fingerprint { fingerprint }))
            .flatten()
    };
    let (before, after, owned, reload) = match updated {
        Some(Ended::Updated {
            before,
            after,
            owned,
            reload,
            ..
        }) => (fp(before), fp(after), owned.clone(), reload.clone()),
        Some(Ended::Pending(..)) => (None, None, Vec::new(), Some(RELOAD_REQUESTED.into())),
        _ => (None, None, Vec::new(), None),
    };
    OperationRecord {
        id: id.into(),
        resource: op.resource.clone(),
        adapter: op.adapter.clone(),
        class: op.class,
        status,
        started_at: times.as_ref().map(|t| t.0.clone()),
        finished_at: times.map(|t| t.1),
        before,
        after,
        owned_outputs: owned,
        error: detail.map(|d| {
            let text = dfm_model::display::visible(&format!("{} {}", d.code.as_str(), d.detail));
            text.chars().take(1024).collect()
        }),
        reload,
    }
}

/// A service definition the plan hashed, read again through its link within the same bound the
/// adapter used. The error text is the state string a mismatch reports.
fn definition_bytes(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    const LIMIT: u64 = 1 << 20;
    let shown = path.display().to_string();
    let io = |e: std::io::Error| format!("unreadable ({}: {})", shown, e.kind());
    let file = std::fs::File::open(path).map_err(io)?;
    if !file.metadata().map_err(io)?.is_file() {
        return Err(format!("not a regular file ({shown})"));
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes).map_err(io)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > LIMIT {
        return Err(format!("over 1 MiB ({shown})"));
    }
    Ok(bytes)
}

/// Checks the gates and then runs the plan. This is the engine's one entry point. A refusal
/// here means nothing was changed. Once the run starts it always returns a [`Report`], also
/// when the journal cannot be read or a step fails, so the caller can still write a receipt.
///
/// The first failed, blocked or unknown operation stops all further mutation: operations that
/// depend on it (through `after`, transitively through results) are `blocked`, every other
/// operation that has not started is `not_started`. Operations that need nothing (`current`,
/// `not_in_scope`) are still reported as they are. There is no rollback.
pub fn apply(run: &Run<'_>, plan: &Plan) -> Result<Report, Refusal> {
    let g = gates(plan, run.generator);
    if let Some(r) = g.refusal {
        return Err(r);
    }
    check_allowances(&g, run.allow, run.allow_adopt).map_err(|mut all| all.remove(0))?;
    check_journal_capacity(
        run.store.journal_len()?,
        run.store.settlement_entries()?,
        plan,
    )?;
    Ok(execute(run, plan))
}

fn execute(run: &Run<'_>, plan: &Plan) -> Report {
    // Without a clock no time stamp is invented: the run does not start.
    let clock = now_ns();
    let mut ex = Exec {
        run,
        plan,
        repaired: 0,
        warnings: Vec::new(),
        attempts: BTreeMap::new(),
        unknown_paths: BTreeMap::new(),
        unknown_resources: BTreeMap::new(),
        clock: Cell::new(*clock.as_ref().unwrap_or(&0)),
    };
    // A failure to settle earlier work stops the run before its first operation.
    // A signal that came before settling wins, and settling (a journal write) is skipped.
    let mut stopped: Option<Refusal> = crate::signal::requested()
        .or(clock.err())
        .or_else(|| settle(&mut ex).err());
    // A pending operation does not stop the run (other operations are independent of it); it
    // decides the exit (5) when nothing else does.
    // shortcut: a dependent of a pending operation is not marked blocked_pending, upgrade by
    // mapping `results == Pending` to Status::BlockedPending when DFM-30 adds the first
    // operation that runs after a service.
    let mut pending: Option<Refusal> = None;
    let mut results: BTreeMap<&str, Status> = BTreeMap::new();
    let mut records = Vec::new();
    for op in &plan.operations {
        // A signal stops new work before the next intent entry. The operation in flight is done.
        stopped = stopped.or_else(crate::signal::requested);
        let (status, rec) = match op.status {
            PlanStatus::NotInScope => (
                Status::NotInScope,
                record(op, &op.journal_id, Status::NotInScope, None, None, None),
            ),
            PlanStatus::Current | PlanStatus::Held | PlanStatus::Blocked | PlanStatus::Planned
                if stopped.is_some()
                    && (!matches!(op.status, PlanStatus::Current | PlanStatus::Held)
                        || ex.clock.get() == 0) =>
            {
                let dependent = op.after.iter().any(|a| {
                    matches!(
                        results.get(a.as_str()),
                        Some(Status::Failed | Status::Blocked | Status::Unknown)
                    )
                });
                let s = if dependent {
                    Status::Blocked
                } else {
                    Status::NotStarted
                };
                let why = dependent
                    .then(|| Refusal::new(Code::PlanStale, "a dependency did not complete"));
                let times = dependent.then(|| (ex.now(), ex.now()));
                (s, record(op, &op.journal_id, s, times, why.as_ref(), None))
            }
            _ => {
                let started = ex.now();
                // A held tool (docs/toolchains.md) needs nothing, like a current one, and keeps
                // its own status in the receipt.
                let settled = if op.status == PlanStatus::Held {
                    Status::Held
                } else {
                    Status::Current
                };
                let ended = if matches!(op.status, PlanStatus::Current | PlanStatus::Held) {
                    // A provider listing does not settle an interrupted install's journal entry.
                    if op.path.is_none() {
                        ex.unknown_install(op).err().unwrap_or(Ended::Current)
                    } else if stopped.is_none() && op.action == Action::Remove {
                        // A stale ledger entry is dropped only while the run is still going.
                        ex.prune(op)
                    } else {
                        Ended::Current
                    }
                } else {
                    ex.run_op(op)
                };
                let times = Some((started, ex.now()));
                match &ended {
                    Ended::Current => (
                        settled,
                        record(op, &op.journal_id, settled, times, None, None),
                    ),
                    Ended::Updated { id, .. } => (
                        Status::Updated,
                        record(op, id, Status::Updated, times, None, Some(&ended)),
                    ),
                    Ended::Pending(r, id) => {
                        pending.get_or_insert_with(|| r.clone());
                        (
                            Status::Pending,
                            record(op, id, Status::Pending, times, Some(r), Some(&ended)),
                        )
                    }
                    Ended::NotStarted(r) => {
                        stopped.get_or_insert_with(|| r.clone());
                        (
                            Status::NotStarted,
                            record(op, &op.journal_id, Status::NotStarted, None, None, None),
                        )
                    }
                    Ended::Failed(r, id) => {
                        stopped.get_or_insert_with(|| r.clone());
                        let id = id.as_deref().unwrap_or(&op.journal_id);
                        (
                            Status::Failed,
                            record(op, id, Status::Failed, times, Some(r), None),
                        )
                    }
                    Ended::Blocked(r) => {
                        stopped.get_or_insert_with(|| r.clone());
                        (
                            Status::Blocked,
                            record(op, &op.journal_id, Status::Blocked, times, Some(r), None),
                        )
                    }
                    Ended::Unknown(r, id) => {
                        stopped.get_or_insert_with(|| r.clone());
                        let id = id.as_deref().unwrap_or(&op.journal_id);
                        (
                            Status::Unknown,
                            record(op, id, Status::Unknown, times, Some(r), None),
                        )
                    }
                }
            }
        };
        results.insert(&op.id, status);
        records.push(rec);
    }
    // A signal after the last operation, or after a failure, still decides the exit
    // (docs/apply.md).
    Report {
        operations: records,
        stopped: crate::signal::requested().or(stopped).or(pending),
        repaired: ex.repaired,
        warnings: ex.warnings,
    }
}
