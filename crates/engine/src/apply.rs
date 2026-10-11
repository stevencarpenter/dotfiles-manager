//! Apply: executes a plan's operations against HOME (docs/apply.md).
//!
//! The caller re-plans under the manager lock and compares the approved digest before it gets
//! here. This module then re-checks, for each operation and immediately before its effect, every
//! fact the plan recorded (`path:` and `ledger:` preconditions) and the topology of the parent
//! chain, walked from the opened HOME one component at a time without following a link. The
//! effect itself uses a primitive that cannot replace or follow what it did not verify
//! ([`crate::publish`]).
//!
//! Order of one operation: prerequisites, intent entry, effect, read-back, outcome entry, ledger
//! update. The receipt is written by the caller after the last operation.

use crate::fail::Fail;
use crate::fsys::{Dir, Peek, Sys};
use crate::observe::observe;
use crate::publish::Publish;
use crate::store::{Probe, Store, now_unix};
use dfm_model::digest::sha256_hex;
use dfm_model::fold;
use dfm_model::plan::{
    Action, Operation, Owned, Plan, Request, Status as PlanStatus, ledger_state, link_fingerprint,
    node_state,
};
use dfm_model::state::{
    Class, Fingerprint, JournalEntry, Ledger, LedgerResource, OperationRecord, OwnedOutput, Phase,
    Recovery, RecoveryOutput, Status, rfc3339, unfinished, valid_id,
};
use dfm_model::{Code, Refusal};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// A published file is at most this large (the same bound the planner hashes).
const MAX_FILE: u64 = dfm_model::plan::MAX_HASHED_BYTES;

/// The consumer reload state recorded for every published output. No reloader exists yet, so a
/// published file never implies a reloaded consumer.
pub const RELOAD_NOT_ATTEMPTED: &str = "not_attempted";

/// Produces the bytes of a generated file. Adapters (DFM-20) implement it. Apply calls it only
/// for an operation it supports, and the content must follow from the operation's recorded
/// inputs: `generate` must itself re-verify the `source:` preconditions.
pub trait Generator {
    fn supports(&self, op: &Operation) -> bool;
    fn generate(&self, op: &Operation) -> Result<Vec<u8>, Refusal>;
}

/// The generator of a build with no adapters: every file operation is unsupported.
#[derive(Debug, Default)]
pub struct NoGenerator;

impl Generator for NoGenerator {
    fn supports(&self, _op: &Operation) -> bool {
        false
    }
    fn generate(&self, op: &Operation) -> Result<Vec<u8>, Refusal> {
        Err(Refusal::new(
            Code::GeneratorUnavailable,
            format!("no generator for {}", op.adapter),
        ))
    }
}

pub struct Run<'a> {
    pub store: &'a Store,
    pub sys: Sys,
    /// The opened HOME. Every effect is relative to it.
    pub home: &'a Dir,
    /// HOME as the layout spells it. Ledger and journal paths use this spelling.
    pub home_path: &'a Path,
    pub generator: &'a dyn Generator,
    /// Operation ids the caller approved as disruptive (`--allow-disruptive`).
    pub allow: &'a BTreeSet<String>,
    pub fail: &'a Fail,
}

/// What happened to every operation of the plan, in plan order.
#[derive(Debug)]
pub struct Report {
    pub operations: Vec<OperationRecord>,
    /// The first operation failure or block, if any. Later operations are blocked or not started.
    pub stopped: Option<Refusal>,
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
                        format!(
                            "ledger path {} is outside HOME",
                            dfm_model::display::visible(&o.path)
                        ),
                    )
                })?;
            out.push(Owned {
                resource: r.resource.clone(),
                adapter: r.adapter.clone(),
                class: r.class,
                path: rel.into(),
                fingerprint: o.fingerprint.clone(),
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
fn components(rel: &str) -> Result<Vec<&str>, Refusal> {
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

fn is_file_action(a: Action) -> bool {
    matches!(a, Action::CreateFile | Action::UpdateFile)
}

/// Gates that need no write. A refusal here means nothing was changed (exit 3).
pub fn preflight(run: &Run<'_>, plan: &Plan) -> Result<(), Refusal> {
    if let Some(b) = plan.blockers.first() {
        return Err(b.refusal());
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut removals = Vec::new();
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
        components(path)?;
        match op.action {
            Action::CreateLink | Action::ReplaceLink => {
                let link = op
                    .link
                    .as_deref()
                    .ok_or_else(|| plan_invalid(format!("{} has no link target", op.id)))?;
                let root = plan.sources.first().map(|s| s.root.as_str());
                let inside = root.is_some_and(|r| {
                    link.strip_prefix(r)
                        .is_some_and(|rest| rest.starts_with('/'))
                });
                if !inside || link.contains('\0') || link.split('/').any(|c| c == "..") {
                    return Err(plan_invalid(format!(
                        "{} links to {link:?}, which is not inside the source root",
                        op.id
                    )));
                }
            }
            Action::CreateFile | Action::UpdateFile => {
                if op.class == Class::Sensitive {
                    return Err(Refusal::new(
                        Code::GeneratorUnavailable,
                        format!(
                            "{}: sensitive outputs are not published yet (no conflict detection)",
                            op.id
                        ),
                    ));
                }
                if !run.generator.supports(op) {
                    return Err(Refusal::new(
                        Code::GeneratorUnavailable,
                        format!(
                            "{}: no generator produces {} content in this build; nothing was changed",
                            op.id, op.adapter
                        ),
                    ));
                }
            }
            Action::Remove => removals.push(op.id.as_str()),
            Action::CreateDir => {}
            Action::None => {
                return Err(plan_invalid(format!("{} is planned with no action", op.id)));
            }
        }
    }
    if let Some(bad) = run.allow.iter().find(|a| !removals.contains(&a.as_str())) {
        return Err(Refusal::new(
            Code::Usage,
            format!("--allow-disruptive {bad} names no removal in this plan"),
        ));
    }
    let missing: Vec<&str> = removals
        .iter()
        .copied()
        .filter(|id| !run.allow.contains(*id))
        .collect();
    if !missing.is_empty() {
        let shown: Vec<String> = missing
            .iter()
            .take(10)
            .map(|m| dfm_model::display::visible(m))
            .collect();
        return Err(Refusal::new(
            Code::DisruptiveUnapproved,
            format!(
                "{} removal(s) need --allow-disruptive <operation-id>: {}{}",
                missing.len(),
                shown.join(", "),
                if missing.len() > shown.len() {
                    ", ..."
                } else {
                    ""
                }
            ),
        ));
    }
    Ok(())
}

struct Exec<'a, 'r> {
    run: &'a Run<'r>,
    /// Journal intents per planner journal id, so an id is never reused after a partial apply.
    attempts: BTreeMap<String, u32>,
    /// Paths an interrupted operation left in an unknown state. Nothing is done to them.
    unknown_paths: BTreeSet<String>,
}

/// How one operation ended.
enum Ended {
    Updated {
        id: String,
        before: Option<String>,
        after: Option<String>,
        owned: Vec<String>,
        reload: Option<String>,
    },
    Current,
    Failed(Refusal),
    Blocked(Refusal),
}

fn blocked(code: Code, detail: impl Into<String>) -> Ended {
    Ended::Blocked(Refusal::new(code, detail))
}

fn failed(r: Refusal) -> Ended {
    Ended::Failed(r)
}

fn now() -> Result<String, Refusal> {
    Ok(rfc3339(now_unix()?))
}

fn step_name(op: &Operation) -> &'static str {
    match op.action {
        Action::CreateDir => "create-dir",
        Action::CreateLink => "create-link",
        Action::ReplaceLink => "replace-link",
        Action::CreateFile => "create-file",
        Action::UpdateFile => "update-file",
        Action::Remove => "remove",
        Action::None => "forget",
    }
}

/// Facts read again immediately before the effect.
struct Facts {
    /// The fingerprint the ledger records for this path now.
    ledger_fp: Option<String>,
}

impl Exec<'_, '_> {
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

    fn entry(
        &self,
        op: &Operation,
        id: &str,
        phase: Phase,
        recovery: Option<Recovery>,
        reason: Option<String>,
    ) -> Result<JournalEntry, Refusal> {
        Ok(JournalEntry {
            schema: 1,
            op_id: id.into(),
            op: step_name(op).into(),
            phase,
            at: now()?,
            resource: op.resource.clone(),
            adapter: op.adapter.clone(),
            class: op.class,
            recovery,
            token: None,
            reason,
        })
    }

    fn journal(
        &self,
        op: &Operation,
        id: &str,
        phase: Phase,
        recovery: Option<Recovery>,
        reason: Option<String>,
    ) -> Result<(), Refusal> {
        self.run
            .store
            .append_journal(&self.entry(op, id, phase, recovery, reason)?)
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
        let ledger = self.run.store.read_ledger().map_err(failed)?;
        let owned = owned_outputs(&ledger, self.run.home_path).map_err(failed)?;
        let entry = owned.iter().find(|o| fold(&o.path) == fold(rel));
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

    fn run_op(&mut self, op: &Operation) -> Ended {
        match self.try_op(op) {
            Ok(e) | Err(e) => e,
        }
    }

    fn try_op(&mut self, op: &Operation) -> Result<Ended, Ended> {
        let rel = op
            .path
            .as_deref()
            .ok_or_else(|| failed(plan_invalid("no path")))?;
        let comps = components(rel).map_err(failed)?;
        let abs = self.abs(rel).map_err(failed)?;
        if self.unknown_paths.iter().any(|u| {
            let (a, b) = (u.as_str(), abs.as_str());
            a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
        }) {
            return Err(blocked(
                Code::OpInterrupted,
                format!("{rel}: an interrupted operation left it in an unknown state"),
            ));
        }
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
        let (old_target, old_file_fp) = match (op.action, &live) {
            (Action::CreateDir | Action::CreateLink | Action::CreateFile, Peek::Missing) => {
                (None, None)
            }
            (Action::CreateDir | Action::CreateLink | Action::CreateFile, _) => {
                return Err(appeared(
                    "appeared before it could be created; it is left alone",
                ));
            }
            (Action::ReplaceLink, Peek::Link { target, .. }) => (Some(target.clone()), None),
            (Action::UpdateFile, Peek::File { .. }) => (None, facts.ledger_fp.clone()),
            (Action::Remove, Peek::File { .. } | Peek::Link { .. }) => {
                if op.class == Class::Sensitive {
                    return Err(appeared(
                        "is sensitive; the manager cannot verify it, so it is kept",
                    ));
                }
                (None, None)
            }
            (Action::ReplaceLink | Action::UpdateFile | Action::Remove, _) => {
                return Err(appeared(
                    "is no longer the entry the plan found; it is left alone",
                ));
            }
            (Action::None, _) => return Err(failed(plan_invalid("no action"))),
        };
        // Content and fingerprints are decided before the intent is written, so a generator
        // failure leaves no interruption record.
        let bytes = if is_file_action(op.action) {
            let b = self.run.generator.generate(op).map_err(failed)?;
            if u64::try_from(b.len()).unwrap_or(u64::MAX) > MAX_FILE {
                return Err(failed(Refusal::new(
                    Code::LimitExceeded,
                    format!("{rel}: generated content exceeds 16 MiB"),
                )));
            }
            Some(b)
        } else {
            None
        };
        let new_fp = match (&bytes, op.link.as_deref()) {
            (Some(b), _) => Some(sha256_hex(b)),
            (None, Some(l)) if matches!(op.action, Action::CreateLink | Action::ReplaceLink) => {
                Some(link_fingerprint(l))
            }
            _ => None,
        };
        let expected = match op.action {
            Action::Remove => facts.ledger_fp.clone(),
            _ => new_fp.clone(),
        };
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
            .map_err(failed)?;
        // From here on the effect may have happened.
        let effect = self.effect(
            op,
            rel,
            &abs,
            &parent,
            name,
            &live,
            old_target.as_deref(),
            old_file_fp.as_deref(),
            bytes.as_deref(),
            new_fp.as_deref(),
        );
        let removed_fp = match effect {
            Ok(fp) => fp,
            Err(e) => {
                let why: String = dfm_model::display::visible(&e.detail)
                    .chars()
                    .take(1024)
                    .collect();
                let note = self.journal(op, &id, Phase::Failed, None, Some(why.clone()));
                return Err(failed(match note {
                    Ok(()) => e,
                    Err(n) => Refusal::new(
                        e.code,
                        format!("{why}; the failure could not be journaled: {}", n.detail),
                    ),
                }));
            }
        };
        // The effect is read back. Now the outcome entry, then ownership.
        self.journal(op, &id, Phase::Done, None, None)
            .map_err(failed)?;
        self.run.fail.hit("apply-after-done").map_err(failed)?;
        let published = matches!(
            op.action,
            Action::CreateLink | Action::ReplaceLink | Action::CreateFile | Action::UpdateFile
        );
        match op.action {
            Action::Remove => self.forget_output(op, &id, &abs).map_err(failed)?,
            _ if published => {
                let fp = new_fp.clone().unwrap_or_default();
                let mode = if bytes.is_some() {
                    op.mode.unwrap_or(0o600)
                } else {
                    0o777
                };
                self.record_output(op, &id, &abs, &fp, mode)
                    .map_err(failed)?;
            }
            _ => {}
        }
        Ok(Ended::Updated {
            id,
            before: match op.action {
                Action::Remove => removed_fp,
                _ => old_file_fp.or_else(|| old_target.as_deref().map(link_fingerprint)),
            },
            after: new_fp,
            owned: if published { vec![abs] } else { Vec::new() },
            reload: published.then(|| RELOAD_NOT_ATTEMPTED.to_string()),
        })
    }

    /// The effect, then its read-back. For a removal it returns the fingerprint of what was
    /// removed. The primitives re-verify the leaf themselves (create never replaces, replace
    /// and update compare the live link target or content hash, remove checks identity).
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
        old_target: Option<&str>,
        old_file_fp: Option<&str>,
        bytes: Option<&[u8]>,
        new_fp: Option<&str>,
    ) -> Result<Option<String>, Refusal> {
        let sys = &self.run.sys;
        let fail = self.run.fail;
        fail.hit("apply-before-effect")?;
        let mut removed = None;
        match op.action {
            Action::CreateDir => parent.make_dir(name)?,
            Action::CreateLink => parent.make_link(name, op.link.as_deref().unwrap_or_default())?,
            Action::ReplaceLink => parent.replace_link(
                name,
                op.link.as_deref().unwrap_or_default(),
                old_target.unwrap_or_default(),
                fail,
            )?,
            Action::CreateFile | Action::UpdateFile => {
                let how = match old_file_fp {
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
            Action::None => return Err(plan_invalid("no action")),
        }
        fail.hit("apply-after-effect")?;
        let bad = |what: &str| {
            Refusal::new(
                Code::IoFailed,
                format!("postcondition failed for {rel}: {what}"),
            )
        };
        match (op.action, parent.peek(sys, name)?) {
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
        parent.remove_entry(name)?;
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
        let mut ledger = self.run.store.read_ledger()?;
        let mut outs = ledger
            .resources
            .iter()
            .find(|r| r.resource == op.resource)
            .map(|r| r.owned_outputs.clone())
            .unwrap_or_default();
        outs.retain(|o| o.path != abs);
        outs.push(OwnedOutput {
            path: abs.into(),
            mode,
            fingerprint: Some(fp.into()),
        });
        outs.sort_by(|a, b| a.path.cmp(&b.path));
        ledger.record(LedgerResource {
            resource: op.resource.clone(),
            adapter: op.adapter.clone(),
            class: op.class,
            op_id: id.into(),
            owned_outputs: outs,
        })?;
        self.run.store.write_ledger(&ledger)
    }

    /// Drops one output from the ledger, and the resource when it owns nothing else.
    fn forget_output(&self, op: &Operation, id: &str, abs: &str) -> Result<(), Refusal> {
        let mut ledger = self.run.store.read_ledger()?;
        for r in &mut ledger.resources {
            if r.resource == op.resource {
                r.owned_outputs.retain(|o| o.path != abs);
                r.op_id = id.into();
            }
        }
        ledger.resources.retain(|r| !r.owned_outputs.is_empty());
        self.run.store.write_ledger(&ledger)
    }

    /// A removal candidate that is already gone: its ledger entry is stale authority. A later
    /// file with the same content at that path must not look owned, so the entry is dropped.
    fn prune(&mut self, op: &Operation) -> Ended {
        let Some(rel) = op.path.as_deref() else {
            return Ended::Current;
        };
        let go = || -> Result<Option<String>, Refusal> {
            let abs = self.abs(rel)?;
            let ledger = self.run.store.read_ledger()?;
            let has = ledger.resources.iter().any(|r| {
                r.resource == op.resource && r.owned_outputs.iter().any(|o| o.path == abs)
            });
            Ok(has.then_some(abs))
        };
        let abs = match go() {
            Ok(Some(a)) => a,
            Ok(None) => return Ended::Current,
            Err(e) => return failed(e),
        };
        let id = self.attempt_id(op);
        let recovery = Recovery {
            outputs: vec![RecoveryOutput {
                path: abs.clone(),
                existed_before: false,
                expected_sha256: None,
            }],
            snapshot: None,
        };
        let r = self
            .journal(op, &id, Phase::Intent, Some(recovery), None)
            .and_then(|()| self.forget_output(op, &id, &abs))
            .and_then(|()| self.journal(op, &id, Phase::Done, None, None));
        match r {
            Ok(()) => Ended::Current,
            Err(e) => failed(e),
        }
    }

    /// Answers an interrupted operation's `already_done` question from the live entry, read
    /// through the opened HOME. It decides nothing about ownership.
    fn probe(&self, e: &JournalEntry) -> Probe {
        let Some(out) = e.recovery.as_ref().and_then(|r| r.outputs.first()) else {
            return Probe::Unknown;
        };
        let Some(rel) = Path::new(&out.path)
            .strip_prefix(self.run.home_path)
            .ok()
            .and_then(Path::to_str)
        else {
            return Probe::Unknown;
        };
        let Ok(comps) = components(rel) else {
            return Probe::Unknown;
        };
        let Some((name, parents)) = comps.split_last() else {
            return Probe::Unknown;
        };
        let (sys, home) = (&self.run.sys, self.run.home);
        let live = home
            .open_below(sys, parents)
            .and_then(|p| p.peek(sys, name).map(|l| (p, l)));
        let Ok((parent, live)) = live else {
            return Probe::Unknown;
        };
        let fp = match &live {
            Peek::File { .. } => parent
                .hash_file(name, MAX_FILE)
                .ok()
                .flatten()
                .map(|(h, _)| h),
            Peek::Link { target, .. } => Some(link_fingerprint(target)),
            _ => None,
        };
        match (e.op.as_str(), &live) {
            ("forget", _) => Probe::Unknown,
            ("remove", Peek::Missing) => Probe::Done,
            ("remove", _) if fp.is_some() && fp == out.expected_sha256 => Probe::NotDone,
            ("create-dir", Peek::Dir { .. }) => Probe::Done,
            (_, Peek::Missing) => Probe::NotDone,
            ("create-link" | "replace-link" | "create-file" | "update-file", _)
                if fp.is_some() && fp == out.expected_sha256 =>
            {
                Probe::Done
            }
            _ => Probe::Unknown,
        }
    }
}

/// Journal entries of earlier runs that never reached a terminal entry are settled first
/// (architecture section 5): the operation's `already_done` check decides `done` or `failed`,
/// or `unknown`, which blocks work on that path. Nothing is run again and no ownership is
/// created.
fn settle(ex: &mut Exec<'_, '_>) -> Result<(), Refusal> {
    let entries = ex.run.store.read_journal()?;
    let pending: Vec<(String, String, Vec<String>)> = unfinished(&entries)
        .into_iter()
        .map(|e| {
            let paths = e
                .recovery
                .iter()
                .flat_map(|r| r.outputs.iter().map(|o| o.path.clone()))
                .collect();
            (e.op_id.clone(), e.op.clone(), paths)
        })
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    let settled = ex.run.store.reconcile(&|e| ex.probe(e))?;
    for (id, op, phase) in settled {
        if phase == Phase::Unknown
            && let Some((_, _, paths)) = pending.iter().find(|(i, o, _)| *i == id && *o == op)
        {
            ex.unknown_paths.extend(paths.iter().cloned());
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

/// Executes the plan in its order. Call [`preflight`] first and re-plan under the lock.
///
/// The first failed or blocked operation stops all further mutation: operations that depend on
/// it (through `after`) are `blocked`, every other operation that has not started is
/// `not_started`. Operations that need nothing (`current`, `not_in_scope`) are still reported
/// as they are. There is no rollback.
pub fn apply(run: &Run<'_>, plan: &Plan) -> Result<Report, Refusal> {
    let mut ex = Exec {
        run,
        attempts: BTreeMap::new(),
        unknown_paths: BTreeSet::new(),
    };
    settle(&mut ex)?;
    let entries = run.store.read_journal()?;
    for op in &plan.operations {
        let prefix = format!("{}-", op.journal_id);
        let used = entries
            .iter()
            .filter(|e| e.phase == Phase::Intent && e.op_id.starts_with(&prefix))
            .count();
        ex.attempts.insert(
            op.journal_id.clone(),
            u32::try_from(used).unwrap_or(u32::MAX),
        );
    }
    let mut results: BTreeMap<&str, Status> = BTreeMap::new();
    let mut records = Vec::new();
    let mut stopped: Option<Refusal> = None;
    for op in &plan.operations {
        let (status, rec) = match op.status {
            PlanStatus::NotInScope => (
                Status::NotInScope,
                record(op, &op.journal_id, Status::NotInScope, None, None, None),
            ),
            PlanStatus::Current | PlanStatus::Blocked | PlanStatus::Planned
                if stopped.is_some() && op.status != PlanStatus::Current =>
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
                let times = dependent
                    .then(|| now().map(|t| (t.clone(), t)))
                    .transpose()?;
                (s, record(op, &op.journal_id, s, times, why.as_ref(), None))
            }
            _ => {
                let started = now()?;
                let ended = if op.status == PlanStatus::Current {
                    // A stale ledger entry is dropped only while the run is still going.
                    if stopped.is_none() && op.id.starts_with("remove@") {
                        ex.prune(op)
                    } else {
                        Ended::Current
                    }
                } else {
                    ex.run_op(op)
                };
                let times = Some((started, now()?));
                match &ended {
                    Ended::Current => (
                        Status::Current,
                        record(op, &op.journal_id, Status::Current, times, None, None),
                    ),
                    Ended::Updated { id, .. } => (
                        Status::Updated,
                        record(op, id, Status::Updated, times, None, Some(&ended)),
                    ),
                    Ended::Failed(r) => {
                        stopped.get_or_insert_with(|| r.clone());
                        (
                            Status::Failed,
                            record(op, &op.journal_id, Status::Failed, times, Some(r), None),
                        )
                    }
                    Ended::Blocked(r) => {
                        stopped.get_or_insert_with(|| r.clone());
                        (
                            Status::Blocked,
                            record(op, &op.journal_id, Status::Blocked, times, Some(r), None),
                        )
                    }
                }
            }
        };
        results.insert(&op.id, status);
        records.push(rec);
    }
    Ok(Report {
        operations: records,
        stopped,
    })
}
