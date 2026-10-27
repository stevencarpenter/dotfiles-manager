//! Plan-time facts about the live file of a co-owned merge (docs/adapters.md, `claude-settings/v1`).
//!
//! The planner is pure and never reads a sensitive file. This module reads it, read-only, and
//! gives the planner a verdict and nothing else: the digest of the declared managed values, the
//! names of the settings, and whether the merge may write. No live content, no hash of the live
//! file and no salted hash leaves this function.

use dfm_engine::{Dir, Got, Live, Peek, Store, Sys};
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

/// Where a merge reads the source files of its fragments: the opened source root (or why it
/// could not be opened) and, with `sync`, the opened live root and its path
/// (`generate::read_source`).
pub struct MergeSources<'a> {
    pub read: &'a Result<Dir, Refusal>,
    pub live: Option<(&'a Dir, &'a Path)>,
}

/// What the plan-time reads share.
#[derive(Clone, Copy)]
struct Cx<'a> {
    resolved: &'a Resolved,
    sys: &'a Sys,
    home: &'a Dir,
    /// The opened source root, or why it could not be opened.
    source: &'a Result<Dir, Refusal>,
    /// With `sync`: the opened live root and its path, as `generate::read_source` takes them.
    live: Option<(&'a Dir, &'a Path)>,
    ledger: &'a [Owned],
}

/// A salted token for the content of each fragment, by name. Empty while `state/salt` does not
/// exist or is unusable (the first apply creates it; an unusable one is refused elsewhere).
fn tokens(store: &Store, frags: &BTreeMap<String, Vec<u8>>) -> Vec<(String, String)> {
    let salt = store.salt().ok().flatten();
    frags
        .iter()
        .map(|(n, b)| {
            let t = salt
                .as_deref()
                .map_or_else(String::new, |s| merge::fragment_token(s, n, b));
            (n.clone(), t)
        })
        .collect()
}

/// The fragments the merge of `r` layers over its declared values, by name (byte order), read
/// as apply reads them: below HOME without following a link, regular, owned by the user, one
/// hard link, within the shared 16 MiB bound. The set is the planner's: the files in the
/// directory that match the pattern and that nobody removes, plus the files a selected link
/// resource puts there (their source files, read from the source root). A fragment made by any
/// other kind of resource has no content before apply, so it is refused.
fn read_fragments(
    cx: &Cx<'_>,
    dir: &str,
    pattern: &str,
) -> Result<BTreeMap<String, Vec<u8>>, Refusal> {
    let Cx {
        resolved,
        sys,
        home,
        source,
        live,
        ledger,
    } = *cx;
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
            (Some(src), Ok(root)) => {
                match super::generate::read_source(root, live, sys, &src, MAX_HASHED_BYTES)? {
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
                }
            }
            // The source root could not be opened: that is the cause, not the fragment.
            (Some(_), Err(e)) => return Err(e.clone()),
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
    source: MergeSources<'_>,
    ledger: &[Owned],
) -> BTreeMap<String, MergeFact> {
    let MergeSources { read: source, live } = source;
    let cx = Cx {
        resolved,
        sys,
        home,
        source,
        live,
        ledger,
    };
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
                        fragments: Vec::new(),
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
        let read = match declared.fragment_spec() {
            None => Ok(BTreeMap::new()),
            Some((dir, pattern)) => read_fragments(&cx, dir, pattern),
        };
        let layered = match (&read, declared.fragment_spec()) {
            (Err(e), _) => Err(e.clone()),
            (Ok(_), None) => Ok(declared.clone()),
            (Ok(f), Some(_)) => {
                declared.with_fragments(f.iter().map(|(n, b)| (n.as_str(), b.as_slice())))
            }
        };
        // Another resource's ledger entry on this path decides before anything is read.
        // Another resource's entry blocks. A stale owner (a renamed or removed resource, or a
        // selected one that no longer claims the path) blocks only while the file exists: the
        // planner retires the entry when the file is missing.
        // The check peeks and never reads, because the file can hold credentials.
        let other = ledger
            .iter()
            .find(|o| dfm_model::fold(&o.path) == dfm_model::fold(claim) && o.resource != r.id);
        let stale = other.is_some_and(|o| {
            !resolved.resources.get(&o.resource).is_some_and(|x| {
                x.claims
                    .iter()
                    .any(|c| dfm_model::fold(c) == dfm_model::fold(claim))
            })
        });
        let other = other.filter(|_| !stale || !known_absent(sys, home, claim));
        let verdict = match (&layered, other) {
            (_, Some(o)) => blocked(
                Code::Conflict,
                &if stale {
                    format!(
                        "{claim} is recorded in the ledger as an output of {}, which no longer claims it. Fix: move the file aside, then plan again; the plan retires the old entry and merges into a new file. Merge the old settings back by hand",
                        o.resource
                    )
                } else {
                    format!(
                        "{claim} is recorded in the ledger as an output of {}. Fix: remove that resource's claim on the path first; the manager does not merge into another resource's output",
                        o.resource
                    )
                },
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
        // Fragments are tracked by a salted content token, so the salt must exist. `bind` creates
        // it (and creates a missing one for a store made by an older build).
        let verdict = match (&read, store.salt()) {
            (Ok(f), Ok(None))
                if !f.is_empty() && !matches!(verdict, MergeVerdict::Blocked { .. }) =>
            {
                blocked(
                    Code::StateInvalid,
                    "state/salt does not exist, so the content of the fragments cannot be tracked. Fix: run bind again with the same profile and machine id; it creates the missing salt and changes nothing else",
                )
            }
            _ => verdict,
        };
        let shown = layered.as_ref().unwrap_or(&declared);
        out.insert(
            r.id.clone(),
            MergeFact {
                managed_sha256: declared.managed_sha256(),
                sets: shown.setting_names(),
                handlers: declared.handler_count(),
                fragments: read
                    .as_ref()
                    .map_or_else(|_| Vec::new(), |f| tokens(store, f)),
                verdict,
            },
        );
    }
    out
}

/// The live target, or `Live::Missing` when a parent directory does not exist (a directory to
/// create). A parent that cannot be opened (a link, a file, no permission) is an error, so the
/// verdict is blocked and never a create over a parent that was not read.
fn read_target(sys: &Sys, home: &Dir, parents: &[&str], name: &str) -> Result<Live, Refusal> {
    match home.open_below_opt(sys, parents)? {
        Some(dir) => dir.read_live(sys, name, MAX_HASHED_BYTES),
        None => Ok(Live::Missing),
    }
}

/// Whether the claimed path is known to be absent, without reading it. Any doubt is "not absent".
fn known_absent(sys: &Sys, home: &Dir, claim: &str) -> bool {
    let comps: Vec<&str> = claim.split('/').collect();
    let Some((name, parents)) = comps.split_last() else {
        return false;
    };
    match home.open_below_opt(sys, parents) {
        Ok(None) => true,
        Ok(Some(dir)) => dir
            .peek(sys, name)
            .is_ok_and(|p| matches!(p, Peek::Missing)),
        Err(_) => false,
    }
}

/// The live bytes of the claimed file (`None` when it or a parent directory is absent), or the
/// blocked verdict for a target that must not be merged into.
fn live_bytes(sys: &Sys, home: &Dir, claim: &str) -> Result<Option<Vec<u8>>, MergeVerdict> {
    let comps: Vec<&str> = claim.split('/').collect();
    let Some((name, parents)) = comps.split_last() else {
        return Err(blocked(Code::TargetInvalid, "the target is empty"));
    };
    match read_target(sys, home, parents, name).map_err(|e| refused(&e))? {
        Live::Missing => Ok(None),
        Live::Unusable(why) => Err(blocked(
            Code::Conflict,
            &format!("{claim} {why}. Fix: replace it with a plain regular file of yours"),
        )),
        Live::TooBig => Err(blocked(
            Code::LimitExceeded,
            &format!("{claim} exceeds 16 MiB"),
        )),
        Live::File { mode, .. } if mode & 0o077 != 0 => Err(blocked(
            Code::Conflict,
            &format!(
                "{claim} is mode {mode:o}, open to group or others, and can hold credentials. Fix: run chmod 600 on it, then plan again"
            ),
        )),
        Live::File { bytes, .. } => Ok(Some(bytes)),
    }
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
    let bytes = match live_bytes(sys, home, claim) {
        Ok(b) => b,
        Err(v) => return v,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    /// Opens `locked` again and removes the tree, also when an assertion panics.
    struct Cleanup(PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::set_permissions(self.0.join("locked"), fs::Permissions::from_mode(0o700));
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_parent_that_cannot_be_opened_is_an_error_and_a_missing_parent_is_missing() {
        let d = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("dfm-mergeplan-{}", std::process::id()));
        let _cleanup = Cleanup(d.clone());
        fs::create_dir_all(d.join("locked")).unwrap();
        fs::write(d.join("file"), "x").unwrap();
        fs::set_permissions(d.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let sys = Sys {
            uid: dfm_platform_macos::current_uid().unwrap(),
        };
        let home = Dir::open_home(&sys, &d).unwrap();
        let got = |parents: &[&str]| read_target(&sys, &home, parents, "settings.json");
        assert!(matches!(got(&["absent"]), Ok(Live::Missing)));
        assert!(got(&["file"]).is_err(), "a file is not a parent directory");
        let verdict = |claim: &str| live_bytes(&sys, &home, claim);
        assert!(matches!(verdict("absent/settings.json"), Ok(None)));
        assert!(matches!(
            verdict("file/settings.json"),
            Err(MergeVerdict::Blocked { .. })
        ));
        if sys.uid == 0 {
            // `eprintln!` is captured for a passing test; the real handle is not.
            let _ = writeln!(std::io::stderr(), "SKIPPED: runs as root");
        } else {
            assert!(
                got(&["locked"]).is_err(),
                "an unreadable parent is not a create"
            );
            assert!(
                matches!(
                    verdict("locked/settings.json"),
                    Err(MergeVerdict::Blocked { .. })
                ),
                "an unreadable parent blocks the verdict"
            );
        }
    }
}
