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
use crate::generate::{Gather, Generator, GeneratorInputs, gather, link_source};
use crate::observe::observe;
use crate::publish::Publish;
use crate::recover;
use crate::store::{Store, now_unix};
use dfm_model::digest::sha256_hex;
use dfm_model::fold;
use dfm_model::merge::{Merged, check_foreign, leaf_hashes, salted};
use dfm_model::plan::{
    Action, Operation, Owned, Plan, Request, Status as PlanStatus, file_fingerprint, ledger_state,
    link_fingerprint, node_state,
};
use dfm_model::state::{
    Class, CoOwned, Fingerprint, JournalEntry, Ledger, LedgerResource, OperationRecord,
    OwnedOutput, Phase, Recovery, RecoveryOutput, Status, rfc3339, valid_id,
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

/// The consumer reload state recorded for every published output. No reloader exists yet, so a
/// published file never implies a reloaded consumer.
pub const RELOAD_NOT_ATTEMPTED: &str = "not_attempted";

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

impl Run<'_> {
    /// The ids the caller approved for one kind.
    pub fn allowed(&self, kind: Approval) -> &BTreeSet<String> {
        match kind {
            Approval::Disruptive => self.allow,
            Approval::Adopt => self.allow_adopt,
        }
    }
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
        Action::CreateFile | Action::UpdateFile | Action::ReplaceLink | Action::Retire
    )
}

fn is_file_action(a: Action) -> bool {
    matches!(a, Action::CreateFile | Action::UpdateFile)
}

/// The two approvals a plan can need before apply writes: each names its operations by id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    /// `--allow-disruptive`: deleting an owned output.
    Disruptive,
    /// `--allow-adopt`: recording ownership of an existing link (DFM-45).
    Adopt,
}

impl Approval {
    pub const ALL: [Approval; 2] = [Approval::Disruptive, Approval::Adopt];

    pub fn from_flag(flag: &str) -> Option<Approval> {
        Approval::ALL.into_iter().find(|a| a.flag() == flag)
    }

    /// The approval an operation needs: a removal and an adoption by their action, and the
    /// first write into an existing co-owned file (`takeover`) as an adoption.
    pub fn of(op: &Operation) -> Option<Approval> {
        if op.takeover {
            return Some(Approval::Adopt);
        }
        Approval::ALL.into_iter().find(|a| a.action() == op.action)
    }

    pub fn flag(self) -> &'static str {
        match self {
            Approval::Disruptive => "--allow-disruptive",
            Approval::Adopt => "--allow-adopt",
        }
    }

    pub fn noun(self) -> &'static str {
        match self {
            Approval::Disruptive => "removal",
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

    pub fn action(self) -> Action {
        match self {
            Approval::Disruptive => Action::Remove,
            Approval::Adopt => Action::Adopt,
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
        // A sensitive output has no content hash. Apply admits one kind of sensitive operation,
        // the co-owned merge, which has its own conflict detection (docs/apply.md, Merge). Any
        // other sensitive operation, a directory included, refuses the whole plan.
        let merge = op.class == Class::Sensitive && generator.merges(op);
        if op.class == Class::Sensitive && !merge {
            return Err(Refusal::new(
                Code::SensitiveUnsupported,
                format!(
                    "{}: this sensitive output is not published or removed by this build (only the claude-settings/v1 merge is)",
                    op.id
                ),
            ));
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
            Action::CreateLink | Action::ReplaceLink | Action::Adopt => {
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
                "the journal ({journal_len} bytes) cannot take the entries of {journaled} operations and {settling} entries of earlier work to settle without passing {} bytes. Nothing was changed. Journal rotation is not built (DFM-19)",
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
    /// Journal intents per planner journal id, so an id is never reused after a partial apply.
    attempts: BTreeMap<String, u32>,
    /// Paths whose state is unknown (an interrupted or unconfirmed operation). Nothing more is
    /// done to them in this run.
    unknown_paths: BTreeMap<String, String>,
    /// Resources with an `unknown` entry that has no intent record, so no path is known. Every
    /// operation of the resource is blocked. Resource name to journal id.
    unknown_resources: BTreeMap<String, String>,
    /// The last good clock reading (unix seconds). A clock that fails later in the run reuses
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
        Action::Retire => "retire",
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
        if let Ok(t) = now_unix() {
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
        self.run.store.append_journal(&JournalEntry {
            schema: 1,
            op_id: id.into(),
            op: step_name(op).into(),
            phase,
            at: self.now(),
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
            // Settling an earlier `unknown` just recorded this very link: adoption has nothing
            // left to do, and the operation is not stale.
            let recorded = op.action == Action::Adopt
                && p.subject.starts_with("ledger:")
                && entry.is_some_and(|e| {
                    e.resource == op.resource
                        && op.link.as_deref().map(link_fingerprint).as_deref()
                            == e.fingerprint.as_deref()
                });
            if actual != p.state && recorded {
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
                    "{rel}: resource {} has an unknown outcome (journal id {id}) with no intent record, so no path can be checked. Nothing was changed. Fix: check the machine, then run `dotfiles-manager state resolve {} --as failed`. Only `--as failed` clears it",
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

    fn try_op(&mut self, op: &Operation) -> Result<Ended, Ended> {
        // The gates admit one sensitive kind, the co-owned merge, which has its own path. Nothing
        // below hashes, fingerprints or journals the content of a sensitive path.
        if op.class == Class::Sensitive {
            if self.run.generator.merges(op) {
                return self.try_merge(op);
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
                Action::ReplaceLink | Action::UpdateFile | Action::Remove | Action::Adopt,
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
            (Action::ReplaceLink | Action::UpdateFile | Action::Remove | Action::Adopt, _) => {
                return Err(appeared(
                    "is no longer the entry the plan found; it is left alone",
                ));
            }
            (Action::None | Action::Retire, _) => {
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
        self.journal(op, &id, Phase::Intent, Some(recovery), None)
            .map_err(|e| Ended::Failed(e, Some(id.clone())))?;
        // From here on the effect may have happened.
        let effect = self.effect(
            op,
            rel,
            &abs,
            &parent,
            name,
            &live,
            facts.ledger_fp.as_deref(),
            bytes.as_deref(),
            new_fp.as_deref(),
        );
        let removed_fp = match effect {
            Ok(fp) => fp,
            Err(e) => {
                let unconfirmed = e.code == Code::IoUnconfirmed;
                // The journal keeps escaped text. The refusal keeps the raw text, which the
                // command escapes once when it prints it.
                let raw: String = e.detail.chars().take(1024).collect();
                let why = dfm_model::display::bounded_visible(&e.detail, 1024);
                let phase = if unconfirmed {
                    Phase::Unknown
                } else {
                    Phase::Failed
                };
                let reason = if unconfirmed {
                    format!("{}{why}", recover::UNCONFIRMED)
                } else {
                    why.clone()
                };
                let note = self.journal(op, &id, phase, None, Some(reason));
                let e = match note {
                    Ok(()) => e,
                    Err(n) => Refusal::new(
                        e.code,
                        format!("{raw}; the outcome could not be journaled: {}", n.detail),
                    ),
                };
                return Err(self.after_intent(e, &id, &abs));
            }
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
            reload: published.then(|| RELOAD_NOT_ATTEMPTED.to_string()),
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
                if mode & 0o077 != 0 {
                    return Err(blocked(
                        Code::Conflict,
                        format!(
                            "{rel} is mode {mode:o}, open to group or others, and can hold credentials. Fix: run chmod 600 on it; nothing was changed"
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
        self.journal(op, &id, Phase::Intent, Some(recovery), None)
            .map_err(|e| Ended::Failed(e, Some(id.clone())))?;
        let effect = self.merge_effect(rel, parent, name, read, &merged.bytes, merged.changed);
        if let Err(e) = effect {
            let unconfirmed = e.code == Code::IoUnconfirmed;
            let raw: String = e.detail.chars().take(1024).collect();
            let why = dfm_model::display::bounded_visible(&e.detail, 1024);
            let phase = if unconfirmed {
                Phase::Unknown
            } else {
                Phase::Failed
            };
            let note = self.journal(op, &id, phase, None, Some(why.clone()));
            let e = match note {
                Ok(()) => e,
                Err(n) => Refusal::new(
                    e.code,
                    format!("{raw}; the outcome could not be journaled: {}", n.detail),
                ),
            };
            return Err(self.after_intent(e, &id, abs));
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

    /// Publishes the merged bytes when they differ from the live file, then reads the result
    /// back. The replace is guarded by the identity captured at the re-read, not by a hash of
    /// the whole file. The read-back always runs, also when nothing was written: the bytes must
    /// be the merged bytes, and the mode must be the mode this operation left (the merge mode
    /// after a write, the mode read before for an unchanged file). It compares in memory and
    /// records nothing.
    fn merge_effect(
        &self,
        rel: &str,
        parent: &Dir,
        name: &str,
        read: &MergeRead,
        bytes: &[u8],
        changed: bool,
    ) -> Result<(), Refusal> {
        let fail = self.run.fail;
        fail.hit("apply-before-effect")?;
        if changed {
            let how = match read.stamp {
                Some(s) => Publish::ReplaceStamp(s),
                None => Publish::Create,
            };
            parent.publish_file(name, bytes, dfm_model::merge::MODE, how, MAX_FILE, fail)?;
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
        let want_mode = if changed {
            Some(dfm_model::merge::MODE)
        } else {
            read.mode
        };
        if parent.entry_mode(name)? != want_mode {
            return Err(bad("the mode differs"));
        }
        Ok(())
    }

    /// Records the co-owned file: no content fingerprint, only the salted managed hash and the
    /// entries the manager added.
    fn record_coowned(
        &self,
        op: &Operation,
        id: &str,
        abs: &str,
        coowned: CoOwned,
    ) -> Result<(), Refusal> {
        let mut ledger = self.run.store.read_ledger_lenient()?;
        let mut outs = ledger
            .resources
            .iter()
            .find(|r| r.resource == op.resource)
            .map(|r| r.owned_outputs.clone())
            .unwrap_or_default();
        let want = fold(abs);
        outs.retain(|o| fold(&o.path) != want);
        outs.push(OwnedOutput {
            path: abs.into(),
            mode: dfm_model::merge::MODE,
            fingerprint: None,
            coowned: Some(coowned),
        });
        outs.sort_by(|a, b| a.path.cmp(&b.path));
        ledger.record(LedgerResource {
            resource: op.resource.clone(),
            adapter: op.adapter.clone(),
            class: Class::Sensitive,
            op_id: id.into(),
            owned_outputs: outs,
        })?;
        self.run.store.write_ledger(&ledger)
    }

    /// The outcome entry, then ownership: the ledger changes only after `done`.
    fn settle_outcome(
        &self,
        op: &Operation,
        id: &str,
        abs: &str,
        records: bool,
        generated: bool,
        new_fp: Option<&str>,
    ) -> Result<(), Refusal> {
        self.journal(op, id, Phase::Done, None, None)?;
        self.run.fail.hit("apply-after-done")?;
        match op.action {
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
        }
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
                fail,
            )?,
            Action::CreateFile | Action::UpdateFile => {
                let how = match ledger_fp {
                    Some(h) if op.action == Action::UpdateFile => {
                        Publish::Replace { expected_sha256: h }
                    }
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
            Action::None | Action::Retire => return Err(plan_invalid("no action")),
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
            (Action::CreateLink | Action::ReplaceLink, Peek::Link { target, .. })
                if Some(target.as_str()) == op.link.as_deref() => {}
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
        match parent.peek(&self.run.sys, name)? {
            Peek::Link {
                target, own: true, ..
            } if Some(target.as_str()) == op.link.as_deref() => Ok(()),
            _ => Err(Refusal::new(
                Code::Conflict,
                format!(
                    "{abs} is no longer the link the plan found; it is left alone and not adopted"
                ),
            )),
        }
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
        parent.remove_entry(name, self.run.fail)?;
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
        recover::record_output(self.run.store, who, id, abs, fp, mode)
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

/// Journal entries of earlier runs are settled first (architecture section 5, DFM-19).
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
    let clock = now_unix();
    let mut ex = Exec {
        run,
        plan,
        repaired: 0,
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
            PlanStatus::Current | PlanStatus::Blocked | PlanStatus::Planned
                if stopped.is_some()
                    && (op.status != PlanStatus::Current || ex.clock.get() == 0) =>
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
                let ended = if op.status == PlanStatus::Current {
                    // A stale ledger entry is dropped only while the run is still going.
                    if stopped.is_none() && op.action == Action::Remove {
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
                        Status::Current,
                        record(op, &op.journal_id, Status::Current, times, None, None),
                    ),
                    Ended::Updated { id, .. } => (
                        Status::Updated,
                        record(op, id, Status::Updated, times, None, Some(&ended)),
                    ),
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
    // A signal after the last operation, or after a failure, still decides the exit (section 10).
    Report {
        operations: records,
        stopped: crate::signal::requested().or(stopped),
        repaired: ex.repaired,
    }
}
