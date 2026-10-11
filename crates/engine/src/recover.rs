//! Cross-run recovery of `unknown` outcomes (DFM-19, docs/state.md rule 5).
//!
//! An `unknown` journal entry blocks its path in every run until a probe or the operator
//! settles it. The probe is the operation's `already_done` check, read through the opened HOME.
//! Ownership is recorded for a settled `done` exactly as the original operation would have
//! recorded it, and only when the live fingerprint equals the one the intent recorded.

use crate::apply::{Run, components};
use crate::fsys::{Dir, Peek, Sys};
use crate::store::{Probe, Store, now_unix};
use dfm_model::plan::{MAX_HASHED_BYTES, link_fingerprint};
use dfm_model::state::{
    Class, JournalEntry, LedgerResource, OwnedOutput, Phase, Unresolved, rfc3339, unresolved,
};
use dfm_model::{Code, Refusal};
use std::collections::BTreeMap;
use std::path::Path;

/// What is at a recorded path now.
#[derive(Debug)]
pub struct Live {
    pub peek: Peek,
    /// SHA-256 of a file's content, or `link_fingerprint` of a link's target text.
    pub fingerprint: Option<String>,
}

/// Reads the entry at `abs` through the opened HOME without following a link. `None` means
/// any doubt: a path outside HOME, an unopenable parent, an unreadable file.
pub fn observe_live(sys: &Sys, home: &Dir, home_path: &Path, abs: &str) -> Option<Live> {
    let rel = Path::new(abs).strip_prefix(home_path).ok()?.to_str()?;
    let comps = components(rel).ok()?;
    let (name, parents) = comps.split_last()?;
    let parent = home.open_below(sys, parents).ok()?;
    let peek = parent.peek(sys, name).ok()?;
    let fingerprint = match &peek {
        Peek::File { .. } => parent
            .hash_file(name, MAX_HASHED_BYTES)
            .ok()
            .flatten()
            .map(|(h, _)| h),
        Peek::Link { target, .. } => Some(link_fingerprint(target)),
        _ => None,
    };
    Some(Live { peek, fingerprint })
}

/// Answers an interrupted operation's `already_done` question. It decides nothing about
/// ownership.
pub fn inspect(sys: &Sys, home: &Dir, home_path: &Path, e: &JournalEntry) -> (Probe, Option<Live>) {
    let Some(out) = e.recovery.as_ref().and_then(|r| r.outputs.first()) else {
        return (Probe::Unknown, None);
    };
    let Some(live) = observe_live(sys, home, home_path, &out.path) else {
        return (Probe::Unknown, None);
    };
    let fp = live.fingerprint.as_ref();
    let same = fp.is_some() && fp == out.expected_sha256.as_ref();
    let probe = match (e.op.as_str(), &live.peek) {
        ("remove", Peek::Missing) => Probe::Done,
        ("remove", _) if same => Probe::NotDone,
        ("create-dir", Peek::Dir { .. }) => Probe::Done,
        (_, Peek::Missing) => Probe::NotDone,
        ("create-link" | "replace-link" | "create-file" | "update-file" | "adopt", _) if same => {
            Probe::Done
        }
        _ => Probe::Unknown,
    };
    (probe, Some(live))
}

/// Settles every outstanding `unknown` entry that the probe can now decide, and returns the
/// paths of the ones it cannot, with the journal id that holds each. Nothing is run again.
pub fn recover_unknown(run: &Run<'_>) -> Result<BTreeMap<String, String>, Refusal> {
    let entries = run.store.read_journal_lenient()?;
    let mut blocked = BTreeMap::new();
    for u in unresolved(&entries) {
        let (probe, live) = match u.intent {
            Some(i) => inspect(&run.sys, run.home, run.home_path, i),
            None => (Probe::Unknown, None),
        };
        match probe {
            Probe::Done => finish(run.store, &u, true, live.as_ref(), "recovered: done")?,
            Probe::NotDone => finish(run.store, &u, false, live.as_ref(), "recovered: not done")?,
            Probe::Unknown => {
                for p in u.paths() {
                    blocked.insert(p.to_string(), u.unknown.op_id.clone());
                }
            }
        }
    }
    Ok(blocked)
}

/// Appends `done` or `failed` for an unresolved entry. For `done`, ownership follows the
/// original operation: a published link or file is recorded only when the live fingerprint
/// equals the one the intent expected, and a removal drops the ledger entry of an absent path.
pub fn finish(
    store: &Store,
    u: &Unresolved<'_>,
    done: bool,
    live: Option<&Live>,
    reason: &str,
) -> Result<(), Refusal> {
    store.append_journal(&JournalEntry {
        phase: if done { Phase::Done } else { Phase::Failed },
        at: rfc3339(now_unix()?),
        recovery: None,
        token: None,
        reason: Some(reason.into()),
        ..u.unknown.clone()
    })?;
    if done && u.unknown.class == Class::Normal {
        own(store, u, live)?;
    }
    Ok(())
}

fn own(store: &Store, u: &Unresolved<'_>, live: Option<&Live>) -> Result<(), Refusal> {
    let e = u.unknown;
    let Some(out) = u
        .intent
        .and_then(|i| i.recovery.as_ref())
        .and_then(|r| r.outputs.first())
    else {
        return Ok(());
    };
    let (resource, adapter, class) = (e.resource.as_str(), e.adapter.as_str(), e.class);
    if e.op == "remove" {
        return forget_output(store, resource, &e.op_id, &out.path);
    }
    let (Some(live), Some(want)) = (live, out.expected_sha256.as_deref()) else {
        return Ok(());
    };
    if live.fingerprint.as_deref() != Some(want) {
        return Ok(());
    }
    let mode = match (e.op.as_str(), &live.peek) {
        ("create-file" | "update-file", Peek::File { mode, .. }) => *mode,
        ("create-link" | "replace-link" | "adopt", Peek::Link { .. }) => 0o777,
        _ => return Ok(()),
    };
    record_output(
        store,
        (resource, adapter, class),
        &e.op_id,
        &out.path,
        want,
        mode,
    )
}

/// Adds or replaces one owned output of a resource in the ledger.
pub(crate) fn record_output(
    store: &Store,
    (resource, adapter, class): (&str, &str, Class),
    id: &str,
    abs: &str,
    fp: &str,
    mode: u32,
) -> Result<(), Refusal> {
    let mut ledger = store.read_ledger_lenient()?;
    let mut outs = ledger
        .resources
        .iter()
        .find(|r| r.resource == resource)
        .map(|r| r.owned_outputs.clone())
        .unwrap_or_default();
    outs.retain(|o| o.path != abs);
    outs.push(OwnedOutput {
        path: abs.into(),
        mode,
        fingerprint: Some(fp.into()),
        coowned: None,
    });
    outs.sort_by(|a, b| a.path.cmp(&b.path));
    ledger.record(LedgerResource {
        resource: resource.into(),
        adapter: adapter.into(),
        class,
        op_id: id.into(),
        owned_outputs: outs,
    })?;
    store.write_ledger(&ledger)
}

/// Drops one output from the ledger, and the resource when it owns nothing else.
pub(crate) fn forget_output(
    store: &Store,
    resource: &str,
    id: &str,
    abs: &str,
) -> Result<(), Refusal> {
    let mut ledger = store.read_ledger_lenient()?;
    for r in &mut ledger.resources {
        if r.resource == resource {
            r.owned_outputs.retain(|o| o.path != abs);
            r.op_id = id.into();
        }
    }
    ledger.resources.retain(|r| !r.owned_outputs.is_empty());
    store.write_ledger(&ledger)
}

/// Settles one outstanding entry on the operator's word (`state resolve`). `done` needs the
/// live state to match what the intent expected, else `E_CONFLICT`. `failed` needs nothing.
pub fn resolve(
    store: &Store,
    (sys, home, home_path): (&Sys, &Dir, &Path),
    journal_id: &str,
    as_done: bool,
) -> Result<(), Refusal> {
    let entries = store.read_journal_lenient()?;
    let open = unresolved(&entries);
    let mut hits = open.iter().filter(|u| u.unknown.op_id == journal_id);
    let (Some(u), None) = (hits.next(), hits.next()) else {
        return Err(Refusal::new(
            Code::Usage,
            format!(
                "no single unresolved operation has the journal id {journal_id}. Fix: doctor lists them"
            ),
        ));
    };
    let reason = "resolved by operator";
    if !as_done {
        return finish(store, u, false, None, reason);
    }
    let conflict = |what: &str| {
        Refusal::new(
            Code::Conflict,
            format!(
                "{journal_id}: {what}. Nothing was changed. Fix: restore the path and run again, or use --as failed"
            ),
        )
    };
    let Some(intent) = u.intent else {
        return Err(conflict(
            "the journal has no intent entry for it, so nothing can be checked",
        ));
    };
    let (probe, live) = inspect(sys, home, home_path, intent);
    if probe != Probe::Done {
        return Err(conflict(
            "the live path does not match the result the operation expected",
        ));
    }
    finish(store, u, true, live.as_ref(), reason)
}
