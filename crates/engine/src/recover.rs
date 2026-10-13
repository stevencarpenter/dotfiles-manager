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
    Class, JournalEntry, LedgerResource, OwnedOutput, Phase, RecoveryOutput, Unresolved, rfc3339,
    unfinished, unresolved,
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
    /// `file_fingerprint` of a file's content, or `link_fingerprint` of a link's target text.
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
/// ownership. The manager writes one output per intent, so an intent with none or with more
/// than one is not probed (probing only the first would settle the others unseen): it stays
/// open for the operator.
pub(crate) fn inspect(store: &Store, w: Where<'_>, e: &JournalEntry) -> (Probe, Option<Live>) {
    let Some([out]) = e.recovery.as_ref().map(|r| r.outputs.as_slice()) else {
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
        // Still the content the ledger records: the replace did not happen.
        ("replace-link" | "update-file", _)
            if fp.is_some() && fp == ledger_fp(store, e, &out.path).as_ref() =>
        {
            Probe::NotDone
        }
        _ => Probe::Unknown,
    };
    (probe, Some(live))
}

/// The fingerprint the ledger records for `abs` under the entry's resource. `None` when there
/// is none or the ledger cannot be read.
fn ledger_fp(store: &Store, e: &JournalEntry, abs: &str) -> Option<String> {
    let key = fold(abs);
    store
        .read_ledger_lenient()
        .ok()?
        .resources
        .into_iter()
        .filter(|r| r.resource == e.resource)
        .flat_map(|r| r.owned_outputs)
        .find(|o| fold(&o.path) == key)?
        .fingerprint
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
    /// Staged copies of an interrupted publish that could not be removed.
    pub warnings: Vec<String>,
}

/// How ownership follows a `done`.
pub(crate) enum Own<'a> {
    /// The operator resolved it by hand: the shape rules apply, nothing else.
    Operator,
    /// Automatic settling. The entry must carry [`UNCONFIRMED`], and `known(resource, path)`
    /// must hold: the resource already has a ledger entry, or the current plan has an
    /// operation for that resource and path. The lookup runs before the `done` entry is
    /// written. A ledger that cannot be read fails it, and the entry stays open.
    Auto(&'a dyn Fn(&str, &str) -> Result<bool, Refusal>),
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
    out.warnings = sweep_stages(store, w)?;
    let known = |resource: &str, abs: &str| -> Result<bool, Refusal> {
        Ok(plan_has(resource, abs)
            || store
                .read_ledger_lenient()?
                .resources
                .iter()
                .any(|r| r.resource == resource))
    };
    // An update or replace that finished is closed through `close_done`, which first moves the
    // ledger fingerprint. A failure returns here with the intent still open, so the next run
    // tries again.
    for i in unfinished(&store.read_journal_lenient()?) {
        if !matches!(i.op.as_str(), "update-file" | "replace-link") {
            continue;
        }
        let (probe, live) = inspect(store, w, i);
        if probe == Probe::Done {
            let u = Unresolved {
                intent: Some(i),
                unknown: i,
            };
            close_done(
                store,
                &u,
                live.as_ref(),
                "reconciled: the effect is present",
                Own::Auto(&known),
            )?;
            out.closed.push((i.op_id.clone(), Phase::Done));
            fresh.insert((i.op_id.clone(), i.op.clone()));
        }
    }
    if !unfinished(&store.read_journal_lenient()?).is_empty() {
        for (id, op, phase) in store.reconcile(&|e| inspect(store, w, e).0)? {
            if phase != Phase::Unknown {
                out.closed.push((id.clone(), phase));
            }
            fresh.insert((id, op));
        }
    }
    let entries = store.read_journal_lenient()?;
    for u in unresolved(&entries) {
        let e = u.unknown;
        let decided = match u.intent {
            Some(i) if !fresh.contains(&(e.op_id.clone(), e.op.clone())) => {
                Some(inspect(store, w, i))
            }
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

/// Removes the staged copies that an interrupted attempt left beside the paths of open
/// intents and unknown entries. A staged copy of a sensitive file holds its content, and
/// nothing else would ever remove it. Only the names the attempt made are removed (see
/// `fsys::staged_as`). Returns a message for each one that could not be removed.
fn sweep_stages(store: &Store, (sys, home, home_path): Where<'_>) -> Result<Vec<String>, Refusal> {
    let entries = store.read_journal_lenient()?;
    let mut targets: Vec<(&str, &str)> = unfinished(&entries)
        .into_iter()
        .flat_map(|i| {
            i.recovery
                .iter()
                .flat_map(|r| &r.outputs)
                .map(|o| (i.op_id.as_str(), o.path.as_str()))
        })
        .collect();
    let open = unresolved(&entries);
    targets.extend(
        open.iter()
            .flat_map(|u| u.paths().map(|p| (u.unknown.op_id.as_str(), p))),
    );
    let mut left = Vec::new();
    for (id, abs) in targets {
        let Some(rel) = Path::new(abs)
            .strip_prefix(home_path)
            .ok()
            .and_then(Path::to_str)
        else {
            continue;
        };
        let Ok(comps) = components(rel) else { continue };
        let Some((name, parents)) = comps.split_last() else {
            continue;
        };
        if let Ok(parent) = home.open_below(sys, parents) {
            left.extend(parent.sweep_stages(sys, name, id));
        }
    }
    Ok(left)
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
    // A torn final line (a crash or failed write mid-append) is repaired and journaled first.
    let repaired = store.repair_torn_journal()?;
    let mut settled = settle_all(store, (sys, home, home_path), &|_, _| false)?;
    settled.repaired = repaired;
    Ok(settled)
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
    let e = u.unknown;
    let first = u
        .intent
        .and_then(|i| i.recovery.as_ref())
        .and_then(|r| r.outputs.first());
    // The ledger lookup comes first: if it fails, no terminal entry exists and the operation
    // can be settled again once the ledger is repaired.
    let auto_known = match (&own, first) {
        (Own::Auto(known), Some(out))
            if e.class == Class::Normal && e.op != "remove" && marked(e) =>
        {
            known(&e.resource, &out.path)?
        }
        _ => false,
    };
    // What the live leaf is, if it is the output the intent expected.
    let finished = match (first, live) {
        (Some(out), Some(live)) => finished_output(e, out, live),
        _ => None,
    };
    // An update or replace already owns its path with the old fingerprint. The ledger moves
    // first: a failure leaves no terminal entry, and the next run repairs it.
    if let Some(owned) = &finished
        && matches!(e.op.as_str(), "update-file" | "replace-link")
        && e.class == Class::Normal
        && holds_other_fingerprint(store, &e.resource, owned)?
    {
        record_output(
            store,
            (&e.resource, &e.adapter, e.class),
            &e.op_id,
            owned.clone(),
        )?;
    }
    store.append_journal(&closing(u, Phase::Done, reason)?)?;
    let Some(out) = first else {
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
        Own::Auto(_) => {
            if !auto_known {
                return Ok(());
            }
        }
    }
    let creates = matches!(e.op.as_str(), "create-link" | "create-file");
    let Some(owned) = finished else {
        return Ok(());
    };
    if creates && out.existed_before {
        return Ok(());
    }
    record_output(store, (&e.resource, &e.adapter, e.class), &e.op_id, owned)
}

/// The ledger entry for the output the intent expected, when the live leaf is exactly that
/// and is of the kind the operation makes and owned by the current user.
fn finished_output(e: &JournalEntry, out: &RecoveryOutput, live: &Live) -> Option<OwnedOutput> {
    let want = out.expected_sha256.as_deref()?;
    if live.fingerprint.as_deref() != Some(want) {
        return None;
    }
    Some(OwnedOutput {
        path: out.path.clone(),
        mode: owned_mode(&e.op, &live.peek)?,
        fingerprint: Some(want.into()),
        coowned: None,
    })
}

/// Whether `resource` owns `out.path` with a fingerprint other than `out`'s.
fn holds_other_fingerprint(
    store: &Store,
    resource: &str,
    out: &OwnedOutput,
) -> Result<bool, Refusal> {
    let key = fold(&out.path);
    Ok(store.read_ledger_lenient()?.resources.iter().any(|r| {
        r.resource == resource
            && r.owned_outputs
                .iter()
                .any(|o| fold(&o.path) == key && o.fingerprint != out.fingerprint)
    }))
}

/// The ledger mode of a finished output: the live mode of a file, 0777 for a link to a file.
/// `None` when the leaf is not the kind the operation makes or belongs to another user.
fn owned_mode(op: &str, peek: &Peek) -> Option<u32> {
    match (op, peek) {
        (
            "create-file" | "update-file",
            Peek::File {
                mode, own: true, ..
            },
        ) => Some(*mode),
        (
            "create-link" | "replace-link" | "adopt",
            Peek::Link {
                to: PeekTo::File,
                own: true,
                ..
            },
        ) => Some(0o777),
        _ => None,
    }
}

/// Adds or replaces one owned output of a resource in the ledger.
pub(crate) fn record_output(
    store: &Store,
    (resource, adapter, class): (&str, &str, Class),
    id: &str,
    out: OwnedOutput,
) -> Result<(), Refusal> {
    let mut ledger = store.read_ledger_lenient()?;
    let mut outs = ledger
        .resources
        .iter()
        .find(|r| r.resource == resource)
        .map(|r| r.owned_outputs.clone())
        .unwrap_or_default();
    let want = fold(&out.path);
    outs.retain(|o| fold(&o.path) != want);
    outs.push(out);
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

/// A journal id quoted so a user can paste it into a shell. The id is raw: the command escapes
/// the refusal that holds it once, when it prints it.
pub(crate) fn quoted(id: &str) -> String {
    format!("'{}'", id.replace('\'', "'\\''"))
}

/// The text that tells an operator how to clear an open entry. `multi` is true for an entry
/// with more than one recorded path, which cannot be resolved as done.
pub(crate) fn fix_text(id: &str, multi: bool) -> String {
    let q = quoted(id);
    let done = if multi {
        "(this entry has more than one path and cannot be resolved as done)"
    } else {
        "(the path holds the result the operation expected)"
    };
    format!(
        "Fix: run `dotfiles-manager state recover` (it settles what the machine shows clearly) or apply. If the entry stays open, check the path, then run `dotfiles-manager state resolve {q} --as done` {done} or `--as failed`"
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
    if intent
        .recovery
        .as_ref()
        .is_some_and(|r| r.outputs.len() > 1)
    {
        return Err(conflict(
            "the intent records more than one path, and only one path can be checked. Use --as failed",
        ));
    }
    let (probe, live) = inspect(store, w, intent);
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

#[cfg(test)]
mod tests {
    use super::quoted;

    #[test]
    fn a_quoted_id_is_raw_so_the_command_escapes_it_once() {
        assert_eq!(quoted(r"p\1'x"), r"'p\1'\''x'");
    }
}
