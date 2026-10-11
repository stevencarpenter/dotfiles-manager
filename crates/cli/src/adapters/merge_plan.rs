//! Plan-time facts about the live file of a co-owned merge (docs/adapters.md, `claude-settings/v1`).
//!
//! The planner is pure and never reads a sensitive file. This module reads it, read-only, and
//! gives the planner a verdict and nothing else: the digest of the declared managed values, the
//! names of the settings, and whether the merge may write. No live content, no hash of the live
//! file and no salted hash leaves this function.

use dfm_engine::{Dir, Got, Live, Store, Sys};
use dfm_model::merge::{self, Desired};
use dfm_model::plan::{
    MAX_HASHED_BYTES, MAX_LISTING, MergeFact, MergeVerdict, Owned, in_scope, matches_pattern,
};
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

/// The fragments the merge of `r` layers over its declared values, by name (byte order), read
/// as apply reads them: below HOME without following a link, regular, owned by the user, one
/// hard link, within the shared 16 MiB bound. The set is the planner's: the files in the
/// directory that match the pattern and that nobody removes, plus the files a selected link
/// resource puts there (their source files, read from the source root). A fragment made by any
/// other kind of resource has no content before apply, so it is refused.
fn read_fragments(
    resolved: &Resolved,
    dir: &str,
    pattern: &str,
    sys: &Sys,
    home: &Dir,
    source: Option<&Dir>,
    ledger: &[Owned],
) -> Result<BTreeMap<String, Vec<u8>>, Refusal> {
    let too_much = || Refusal::new(Code::LimitExceeded, "the fragments together exceed 16 MiB");
    let left = std::cell::Cell::new(MAX_HASHED_BYTES);
    let mut out = BTreeMap::new();
    let mut take = |name: String, bytes: Vec<u8>| -> Result<(), Refusal> {
        left.set(
            left.get()
                .checked_sub(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
                .ok_or_else(too_much)?,
        );
        out.insert(name, bytes);
        Ok(())
    };
    let dir_f = dfm_model::fold(dir);
    let mut selected = Vec::new();
    for other in resolved.resources.values().filter(|o| in_scope(o)) {
        for claim in &other.claims {
            let Some((parent, name)) = claim.rsplit_once('/') else {
                continue;
            };
            if dfm_model::fold(parent) == dir_f && matches_pattern(pattern, name) {
                selected.push((other, name.to_string()));
            }
        }
    }
    for (other, name) in &selected {
        let path = format!("{dir}/{name}");
        let src = (other.kind == "link")
            .then(|| {
                other
                    .provenance
                    .values()
                    .find_map(|p| p.source_path.clone())
            })
            .flatten();
        let bytes = match (src, source) {
            (Some(src), Some(root)) => match root.read_input_below(sys, &src, MAX_HASHED_BYTES)? {
                Got::Bytes(b) => b,
                Got::TooBig => return Err(too_much()),
                Got::Absent => {
                    return Err(Refusal::new(
                        Code::SourceUntrusted,
                        format!(
                            "the source of fragment {path} is not a regular file owned by you with one hard link"
                        ),
                    ));
                }
            },
            _ => {
                return Err(Refusal::new(
                    Code::GeneratorUnavailable,
                    format!(
                        "fragment {path} comes from {} ({}), which has no content before apply. Fix: make the fragment a file of yours or a link resource",
                        other.id, other.kind
                    ),
                ));
            }
        };
        take(name.clone(), bytes)?;
    }
    let comps: Vec<&str> = dir.split('/').collect();
    let Some(live_dir) = home.open_below_opt(sys, &comps)? else {
        return Ok(out);
    };
    let listing = live_dir.list_upto(MAX_LISTING)?;
    if listing.odd > 0 || listing.names.len() > MAX_LISTING {
        return Err(Refusal::new(
            Code::Conflict,
            format!("{dir} holds a name that is not UTF-8 or too many entries"),
        ));
    }
    for name in listing.names.iter().filter(|n| matches_pattern(pattern, n)) {
        let path = format!("{dir}/{name}");
        let folded = dfm_model::fold(&path);
        // The planner reports a non-ASCII name itself. A name another resource's output removes
        // is not an input (a deselected producer), and a selected one was read above.
        if !name.is_ascii()
            || selected
                .iter()
                .any(|(_, n)| dfm_model::fold(n) == dfm_model::fold(name))
            || ledger.iter().any(|o| dfm_model::fold(&o.path) == folded)
        {
            continue;
        }
        match live_dir.read_input(sys, name, left.get())? {
            Got::Bytes(b) => take(name.clone(), b)?,
            Got::TooBig => return Err(too_much()),
            Got::Absent => {
                return Err(Refusal::new(
                    Code::Conflict,
                    format!("fragment {path} is not a plain file of yours with one link"),
                ));
            }
        }
    }
    Ok(out)
}

/// The merge facts for every selected merge resource, by resource id. `home_path` is HOME as the
/// layout spells it, which is how the ledger spells its paths. `source` is the opened source
/// root, for the fragments a selected link resource puts in the fragment directory.
pub fn merge_facts(
    resolved: &Resolved,
    store: &Store,
    sys: &Sys,
    home: &Dir,
    home_path: &Path,
    source: Option<&Dir>,
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
        let declared = match Desired::from_value(&r.value) {
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
        // The declared values with the fragments merged over them. A fragment that cannot be
        // read or parsed blocks the operation with its name and no content.
        let layered = match declared.fragment_spec() {
            None => Ok(declared.clone()),
            Some((dir, pattern)) => {
                read_fragments(resolved, dir, pattern, sys, home, source, ledger).and_then(|f| {
                    declared.with_fragments(f.iter().map(|(n, b)| (n.as_str(), b.as_slice())))
                })
            }
        };
        // Another resource's ledger entry on this path decides before anything is read.
        let other = ledger
            .iter()
            .find(|o| dfm_model::fold(&o.path) == dfm_model::fold(claim) && o.resource != r.id);
        let verdict = match (&layered, other) {
            (_, Some(o)) => blocked(
                Code::Conflict,
                &format!(
                    "{claim} is recorded in the ledger as an output of {}. Fix: remove that resource's claim on the path first; the manager does not merge into another resource's output",
                    o.resource
                ),
            ),
            (Err(e), None) => refused(e),
            (Ok(desired), None) => {
                let desired = match home_path.to_str() {
                    Some(h) => desired.clone().with_home(h),
                    None => desired.clone(),
                };
                verdict_for(&r.id, claim, &desired, store, sys, home, home_path)
            }
        };
        let shown = layered.as_ref().unwrap_or(&declared);
        out.insert(
            r.id.clone(),
            MergeFact {
                managed_sha256: declared.managed_sha256(),
                sets: shown.setting_names(),
                handlers: declared.handler_count(),
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
    let salt = match store.salt() {
        Ok(s) => s,
        Err(e) => return refused(&e),
    };
    match merge::classify(
        bytes.as_deref(),
        desired,
        recorded.as_ref(),
        salt.as_deref(),
    ) {
        Ok((_, merge::Outcome::Current)) => MergeVerdict::Current,
        Ok((_, merge::Outcome::Write)) => MergeVerdict::Write,
        Err(e) => refused(&e),
    }
}
