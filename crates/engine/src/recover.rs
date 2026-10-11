//! Cross-run recovery of `unknown` outcomes (DFM-19, docs/state.md rule 5).
//!
//! An `unknown` journal entry blocks its path in every run until a probe or the operator
//! settles it. The probe is the operation's `already_done` check, read through the opened HOME.
//! Settling a `done` records ownership only under the adoption rules (see [`Own`]).

use crate::apply::components;
use crate::fsys::{Dir, Peek, PeekTo, Sys};
use crate::store::{Probe, Store, now_unix};
use dfm_model::fold;
use dfm_model::plan::{MAX_HASHED_BYTES, link_fingerprint};
use dfm_model::state::{
    Class, JournalEntry, LedgerResource, OwnedOutput, Phase, Unresolved, rfc3339, unfinished,
    unresolved,
};
use dfm_model::{Code, Refusal};
use std::collections::BTreeSet;
use std::path::Path;

/// The reason prefix the executor writes on an `unknown` entry whose effect it had observed
/// (the primitive committed and only durability was unconfirmed). Only such an entry may be
/// owned by automatic settling: an entry whose path matches later was not seen to match then.
pub(crate) const UNCONFIRMED: &str = "unconfirmed: ";

/// The opened HOME and what is needed to read through it.
pub(crate) type Where<'a> = (&'a Sys, &'a Dir, &'a Path);

/// What is at a recorded path now.
#[derive(Debug)]
pub(crate) struct Live {
    pub peek: Peek,
    /// SHA-256 of a file's content, or `link_fingerprint` of a link's target text.
    pub fingerprint: Option<String>,
}

/// Reads the entry at `abs` through the opened HOME without following a link. `None` means
/// any doubt: a path outside HOME, an unopenable parent, an unreadable file. A file is hashed
/// only when `hash` is set, so a sensitive path (no expected hash) is never read.
pub(crate) fn observe_live(
    (sys, home, home_path): Where<'_>,
    abs: &str,
    hash: bool,
) -> Option<Live> {
    let rel = Path::new(abs).strip_prefix(home_path).ok()?.to_str()?;
    let comps = components(rel).ok()?;
    let (name, parents) = comps.split_last()?;
    let parent = home.open_below(sys, parents).ok()?;
    let peek = parent.peek(sys, name).ok()?;
    let fingerprint = match &peek {
        Peek::File { .. } if hash => parent
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
pub(crate) fn inspect(w: Where<'_>, e: &JournalEntry) -> (Probe, Option<Live>) {
    let Some(out) = e.recovery.as_ref().and_then(|r| r.outputs.first()) else {
        return (Probe::Unknown, None);
    };
    let Some(live) = observe_live(w, &out.path, out.expected_sha256.is_some()) else {
        return (Probe::Unknown, None);
    };
    let fp = live.fingerprint.as_ref();
    let same = fp.is_some() && fp == out.expected_sha256.as_ref();
    let probe = match (e.op.as_str(), &live.peek) {
        ("remove", Peek::Missing) => Probe::Done,
        ("remove", _) if same => Probe::NotDone,
        (JournalEntry::CREATE_DIR, Peek::Dir { .. }) => Probe::Done,
        (_, Peek::Missing) => Probe::NotDone,
        ("create-link" | "replace-link" | "create-file" | "update-file" | "adopt", _) if same => {
            Probe::Done
        }
        _ => Probe::Unknown,
    };
    (probe, Some(live))
}

/// An `unknown` entry that stays open after settling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Open {
    /// The journal id (`op_id`) to give to `state resolve`.
    pub id: String,
    pub op: String,
    pub resource: String,
    /// The paths the intent recorded. Empty when the journal has no intent entry.
    pub paths: Vec<String>,
    /// Whether the intent recorded at least one path. An intent with an empty list is treated
    /// like a missing intent: nothing can be probed or blocked by path.
    pub has_paths: bool,
}

/// What settling did.
#[derive(Debug, Default)]
pub struct Settled {
    /// Journal id and the phase written (`Done` or `Failed`).
    pub closed: Vec<(String, Phase)>,
    pub open: Vec<Open>,
    /// Bytes of a torn final journal line that were cut off first (0 when none).
    pub repaired: usize,
}

/// How ownership follows a `done`.
pub(crate) enum Own<'a> {
    /// The operator resolved it by hand: the shape rules apply, nothing else.
    Operator,
    /// Automatic settling. The entry must carry [`UNCONFIRMED`], and `known(resource, path)`
    /// must hold: the resource already has a ledger entry, or the current plan has an
    /// operation for that resource and path.
    Auto(&'a dyn Fn(&str, &str) -> bool),
}

/// Settles what the machine shows clearly, in two steps:
/// 1. an `intent` with no terminal entry is probed and journaled (`Store::reconcile`);
/// 2. every `unknown` entry still open, from this call or an earlier run, is probed again,
///    except those step 1 just wrote.
///
/// A clear answer is journaled `done` or `failed`. Nothing is run again.
/// `plan_has(resource, absolute path)` says whether the current plan names that output. A caller
/// with no plan passes a function that returns false.
pub(crate) fn settle_all(
    store: &Store,
    w: Where<'_>,
    plan_has: &dyn Fn(&str, &str) -> bool,
) -> Result<Settled, Refusal> {
    let mut out = Settled::default();
    let mut fresh = BTreeSet::new();
    // A torn final line (a crash or failed write mid-append) is repaired and journaled first.
    out.repaired = store.repair_torn_journal()?;
    if !unfinished(&store.read_journal_lenient()?).is_empty() {
        for (id, op, phase) in store.reconcile(&|e| inspect(w, e).0)? {
            if phase != Phase::Unknown {
                out.closed.push((id.clone(), phase));
            }
            fresh.insert((id, op));
        }
    }
    let known = |resource: &str, abs: &str| {
        plan_has(resource, abs)
            || store
                .read_ledger_lenient()
                .is_ok_and(|l| l.resources.iter().any(|r| r.resource == resource))
    };
    let entries = store.read_journal_lenient()?;
    for u in unresolved(&entries) {
        let e = u.unknown;
        let decided = match u.intent {
            Some(i) if !fresh.contains(&(e.op_id.clone(), e.op.clone())) => Some(inspect(w, i)),
            _ => None,
        };
        match decided {
            Some((Probe::Done, live)) => {
                close_done(
                    store,
                    &u,
                    live.as_ref(),
                    "recovered: done",
                    Own::Auto(&known),
                )?;
                out.closed.push((e.op_id.clone(), Phase::Done));
            }
            Some((Probe::Unknown, Some(live))) if marked(e) && live_mismatch(&live, u.intent) => {
                // The executor had seen the effect when it wrote the entry. A readable file or
                // link that differs now is not that effect, and a later match must not be
                // owned: close it.
                close_failed(store, &u, "recovered: not done; a later match is not owned")?;
                out.closed.push((e.op_id.clone(), Phase::Failed));
            }
            Some((Probe::NotDone, _)) => {
                close_failed(store, &u, "recovered: not done")?;
                out.closed.push((e.op_id.clone(), Phase::Failed));
            }
            _ => out.open.push(Open {
                id: e.op_id.clone(),
                op: e.op.clone(),
                resource: e.resource.clone(),
                paths: u.paths().map(str::to_string).collect(),
                has_paths: u.paths().next().is_some(),
            }),
        }
    }
    Ok(out)
}

fn marked(e: &JournalEntry) -> bool {
    e.reason
        .as_deref()
        .is_some_and(|r| r.starts_with(UNCONFIRMED))
}

/// A readable file or link whose fingerprint differs from the one the intent expected.
fn live_mismatch(live: &Live, intent: Option<&JournalEntry>) -> bool {
    let want = intent
        .and_then(|i| i.recovery.as_ref())
        .and_then(|r| r.outputs.first())
        .and_then(|o| o.expected_sha256.as_deref());
    matches!(live.peek, Peek::File { .. } | Peek::Link { .. })
        && want.is_some()
        && live.fingerprint.is_some()
        && live.fingerprint.as_deref() != want
}

/// Settle-only entry point (`state recover`). With no plan, automatic ownership needs the
/// resource to have a ledger entry already.
pub fn recover(
    store: &Store,
    (sys, home, home_path): (&Sys, &Dir, &Path),
) -> Result<Settled, Refusal> {
    settle_all(store, (sys, home, home_path), &|_, _| false)
}

fn closing(u: &Unresolved<'_>, phase: Phase, reason: &str) -> Result<JournalEntry, Refusal> {
    Ok(JournalEntry {
        phase,
        at: rfc3339(now_unix()?),
        recovery: None,
        token: None,
        reason: Some(reason.into()),
        ..u.unknown.clone()
    })
}

pub(crate) fn close_failed(store: &Store, u: &Unresolved<'_>, reason: &str) -> Result<(), Refusal> {
    store.append_journal(&closing(u, Phase::Failed, reason)?)
}

/// Appends `done`, then handles ownership. A removal drops the ledger entry of the absent
/// path. A created, replaced or adopted output is recorded only when every rule holds:
/// - `Own` allows it (see [`Own`]);
/// - the live fingerprint equals the one the intent expected;
/// - a create did not find the path present before (`existed_before` is false);
/// - the leaf is owned by the current user, and a link resolves to a regular file.
///
/// Otherwise the entry is closed without ownership, and the next plan offers `adopt`.
/// The mode is the live mode of a file (the intent does not carry one) and 0777 for a link.
pub(crate) fn close_done(
    store: &Store,
    u: &Unresolved<'_>,
    live: Option<&Live>,
    reason: &str,
    own: Own<'_>,
) -> Result<(), Refusal> {
    store.append_journal(&closing(u, Phase::Done, reason)?)?;
    let e = u.unknown;
    let Some(out) = u
        .intent
        .and_then(|i| i.recovery.as_ref())
        .and_then(|r| r.outputs.first())
    else {
        return Ok(());
    };
    if e.class != Class::Normal {
        return Ok(());
    }
    if e.op == "remove" {
        return forget_output(store, &e.resource, &e.op_id, &out.path);
    }
    match own {
        Own::Operator => {}
        Own::Auto(known) => {
            if !marked(e) || !known(&e.resource, &out.path) {
                return Ok(());
            }
        }
    }
    let creates = matches!(e.op.as_str(), "create-link" | "create-file");
    let (Some(live), Some(want)) = (live, out.expected_sha256.as_deref()) else {
        return Ok(());
    };
    if (creates && out.existed_before) || live.fingerprint.as_deref() != Some(want) {
        return Ok(());
    }
    let mode = match (e.op.as_str(), &live.peek) {
        (
            "create-file" | "update-file",
            Peek::File {
                mode, own: true, ..
            },
        ) => *mode,
        (
            "create-link" | "replace-link" | "adopt",
            Peek::Link {
                to: PeekTo::File,
                own: true,
                ..
            },
        ) => 0o777,
        _ => return Ok(()),
    };
    record_output(
        store,
        (&e.resource, &e.adapter, e.class),
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
    let want = fold(abs);
    outs.retain(|o| fold(&o.path) != want);
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
    let want = fold(abs);
    for r in &mut ledger.resources {
        if r.resource == resource {
            r.owned_outputs.retain(|o| fold(&o.path) != want);
            r.op_id = id.into();
        }
    }
    ledger.resources.retain(|r| !r.owned_outputs.is_empty());
    store.write_ledger(&ledger)
}

/// A journal id quoted so a user can paste it into a shell.
pub(crate) fn quoted(id: &str) -> String {
    format!(
        "'{}'",
        dfm_model::display::visible(id).replace('\'', "'\\''")
    )
}

/// The text that tells an operator how to clear an open entry.
pub(crate) fn fix_text(id: &str) -> String {
    let q = quoted(id);
    format!(
        "Fix: run `dotfiles-manager state recover` (it settles what the machine shows clearly) or apply. If the entry stays open, check the path, then run `dotfiles-manager state resolve {q} --as done` (the path holds the result the operation expected) or `--as failed`"
    )
}

/// Settles one open entry on the operator's word (`state resolve`). `done` needs the live
/// state to match what the intent expected, else `E_CONFLICT`. `failed` needs nothing. An
/// entry with no intent can only be resolved as `failed`.
pub fn resolve(
    store: &Store,
    w: (&Sys, &Dir, &Path),
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
        return close_failed(store, u, reason);
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
    let (probe, live) = inspect(w, intent);
    if probe != Probe::Done {
        return Err(conflict(
            "the live path does not match the result the operation expected",
        ));
    }
    close_done(store, u, live.as_ref(), reason, Own::Operator)
}

/// Whether two absolute paths are the same, or one lies below the other, ignoring case.
pub(crate) fn related(a: &str, b: &str) -> bool {
    let (a, b) = (fold(a), fold(b));
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}
