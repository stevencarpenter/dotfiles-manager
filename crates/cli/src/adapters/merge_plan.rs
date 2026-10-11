//! Plan-time facts about the live file of a co-owned merge (docs/adapters.md, `claude-settings/v1`).
//!
//! The planner is pure and never reads a sensitive file. This module reads it, read-only, and
//! gives the planner a verdict and nothing else: the digest of the declared managed values, the
//! names of the settings, and whether the merge may write. No live content, no hash of the live
//! file and no salted hash leaves this function.

use dfm_engine::{Dir, Live, Store, Sys};
use dfm_model::merge::{self, Desired};
use dfm_model::plan::{MAX_HASHED_BYTES, MergeFact, MergeVerdict, Owned, in_scope};
use dfm_model::resolve::Resolved;
use dfm_model::state::CoOwned;
use dfm_model::{Code, Refusal};
use std::collections::BTreeMap;
use std::path::Path;

fn blocked(code: Code, text: &str) -> MergeVerdict {
    // The model's messages end with `Fix: ...`. The planner prints the fix on its own.
    let (detail, correction) = text
        .split_once(" Fix: ")
        .unwrap_or((text, "fix the file or the resource value, then plan again"));
    MergeVerdict::Blocked {
        code,
        detail: detail.trim_end_matches('.').to_string(),
        correction: correction.to_string(),
    }
}

fn refused(r: &Refusal) -> MergeVerdict {
    blocked(r.code, &r.detail)
}

/// The merge facts for every selected merge resource, by resource id. `home_path` is HOME as the
/// layout spells it, which is how the ledger spells its paths.
pub fn merge_facts(
    resolved: &Resolved,
    store: &Store,
    sys: &Sys,
    home: &Dir,
    home_path: &Path,
    ledger: &[Owned],
) -> BTreeMap<String, MergeFact> {
    let mut out = BTreeMap::new();
    for r in resolved
        .resources
        .values()
        .filter(|r| in_scope(r) && r.adapter == merge::ADAPTER)
    {
        let [claim] = r.claims.as_slice() else {
            continue;
        };
        let desired = match Desired::from_value(&r.value) {
            Ok(d) => d,
            Err(e) => {
                out.insert(
                    r.id.clone(),
                    MergeFact {
                        managed_sha256: String::new(),
                        sets: Vec::new(),
                        handlers: 0,
                        verdict: blocked(
                            e.code,
                            &format!(
                                "{}. Fix: correct the resource value in {}",
                                e.detail, r.declared_in
                            ),
                        ),
                    },
                );
                continue;
            }
        };
        // Another resource's ledger entry on this path decides before anything is read.
        let other = ledger
            .iter()
            .find(|o| dfm_model::fold(&o.path) == dfm_model::fold(claim) && o.resource != r.id);
        let verdict = match other {
            Some(o) => blocked(
                Code::Conflict,
                &format!(
                    "{claim} is recorded in the ledger as an output of {}. Fix: remove that resource's claim on the path first; the manager does not merge into another resource's output",
                    o.resource
                ),
            ),
            None => verdict_for(&r.id, claim, &desired, store, sys, home, home_path),
        };
        out.insert(
            r.id.clone(),
            MergeFact {
                managed_sha256: desired.managed_sha256(),
                sets: desired.setting_names(),
                handlers: desired.handler_count(),
                verdict,
            },
        );
    }
    out
}

fn verdict_for(
    resource: &str,
    claim: &str,
    desired: &Desired,
    store: &Store,
    sys: &Sys,
    home: &Dir,
    home_path: &Path,
) -> MergeVerdict {
    let comps: Vec<&str> = claim.split('/').collect();
    let Some((name, parents)) = comps.split_last() else {
        return blocked(Code::TargetInvalid, "the target is empty");
    };
    // A parent that is a link or missing is the planner's own finding (E_PARENT_UNSAFE, or a
    // directory to create). Nothing is read through it.
    let live = match home.open_below_opt(sys, parents) {
        Ok(Some(dir)) => match dir.read_live(sys, name, MAX_HASHED_BYTES) {
            Ok(l) => l,
            Err(e) => return refused(&e),
        },
        Ok(None) | Err(_) => Live::Missing,
    };
    let bytes = match live {
        Live::Missing => None,
        Live::Unusable(why) => {
            return blocked(
                Code::Conflict,
                &format!("{claim} {why}. Fix: replace it with a plain regular file of yours"),
            );
        }
        Live::TooBig => return blocked(Code::LimitExceeded, &format!("{claim} exceeds 16 MiB")),
        Live::File { bytes, mode, .. } => {
            if mode & 0o077 != 0 {
                return blocked(
                    Code::Conflict,
                    &format!(
                        "{claim} is mode {mode:o}, open to group or others, and can hold credentials. Fix: run chmod 600 on it, then plan again"
                    ),
                );
            }
            Some(bytes)
        }
    };
    let abs = home_path.join(claim);
    let recorded: Option<CoOwned> = match store.read_ledger_lenient() {
        Ok(l) => l
            .resources
            .iter()
            .filter(|x| x.resource == resource)
            .flat_map(|x| &x.owned_outputs)
            .find(|o| Path::new(&o.path) == abs)
            .and_then(|o| o.coowned.clone()),
        Err(e) => return refused(&e),
    };
    let owned = recorded
        .as_ref()
        .map(|c| c.entries.clone())
        .unwrap_or_default();
    let merged = match merge::merge(bytes.as_deref(), desired, &owned) {
        Ok(m) => m,
        Err(e) => return refused(&e),
    };
    let Some(c) = recorded else {
        // No ledger entry: the manager may write. It takes over the managed fields.
        return MergeVerdict::Write;
    };
    let salt = match store.salt() {
        Ok(Some(s)) => s,
        Ok(None) => {
            return blocked(
                Code::StateInvalid,
                "state/salt is missing but the ledger records a salted hash, so a foreign edit cannot be detected. Fix: restore state/salt from a copy",
            );
        }
        Err(e) => return refused(&e),
    };
    if merge::salted(&salt, &merged.live_view) != c.managed_hash
        && merged.live_view != merged.rendered_view
    {
        return blocked(
            Code::Conflict,
            &format!(
                "a managed field of {claim} was changed outside the manager since the last apply. Fix: restore the declared value in the file, or set the same value in the module so the manager adopts your edit"
            ),
        );
    }
    let recorded_now = merge::salted(&salt, &merged.rendered_view) == c.managed_hash
        && c.entries == merged.entries;
    if !merged.changed && recorded_now {
        MergeVerdict::Current
    } else {
        MergeVerdict::Write
    }
}
