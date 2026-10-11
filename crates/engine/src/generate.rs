//! The generator seam: how apply obtains the bytes of a generated file (docs/adapters.md).
//!
//! An adapter is a [`Generator`]. It never opens a file. Apply reads every input the plan
//! recorded, descriptor-relatively and without following links, compares each with the plan,
//! and hands the verified bytes to the generator in [`GeneratorInputs`]. The same function
//! serves `render` (no inputs beyond the resource value) and `assemble` (a body and the
//! fragments the planner listed).

use crate::fsys::{Dir, Got, Peek, Sys};
use dfm_model::digest::{EntryKind, TreeEntry, sha256_hex, tree_digest};
use dfm_model::fold;
use dfm_model::merge::{Merged, fragment_token};
use dfm_model::plan::{
    MAX_HASHED_BYTES, MAX_LISTING, MAX_SOURCE_ENTRIES, MAX_SOURCE_FILE_BYTES,
    MAX_SOURCE_TOTAL_BYTES, Operation, Plan,
};
use dfm_model::state::{AppendedItem, Class, OwnedEntry};
use dfm_model::{Code, Refusal};
use std::collections::BTreeMap;
use std::path::Path;

/// The verified live input of one generated file.
#[derive(Default, Clone, PartialEq, Eq)]
pub struct GeneratorInputs {
    /// Source files by their path in the source tree, one per entry of `Operation::inputs`.
    /// Each hash equals the plan's `source:` precondition.
    pub sources: BTreeMap<String, Vec<u8>>,
    /// The names now in the fragment directory, sorted. `None` when the operation has no
    /// fragment directory.
    pub listing: Option<Vec<String>>,
    /// The content of each fragment the plan listed, by name. A foreign fragment equals the
    /// hash the planner observed. A selected fragment equals its producer's planned output.
    pub fragments: BTreeMap<String, Vec<u8>>,
    /// The live file of a co-owned merge, read under the writer lock immediately before the
    /// merge. `None` when the file is absent. The bytes may hold credentials in fields the
    /// manager does not own: the adapter must not copy them into any message.
    pub live: Option<Vec<u8>>,
    /// The handler entries the ledger records as added by the manager (merge and retire).
    pub owned_entries: Vec<OwnedEntry>,
    /// The array items the ledger records as appended by the manager (merge and retire).
    pub owned_appended: Vec<AppendedItem>,
    /// The operation is the approved first write into an existing co-owned file with no ledger
    /// entry. A merge then records an identical entry already in place as owned.
    pub takeover: bool,
    /// HOME as the machine spells it, for the merge to recognize an existing hook that spells the
    /// same command with the absolute home path.
    pub home: Option<String>,
}

/// `Debug` prints the size of the live file and never its bytes: it can hold credentials.
impl std::fmt::Debug for GeneratorInputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeneratorInputs")
            .field("sources", &self.sources.keys().collect::<Vec<_>>())
            .field("listing", &self.listing)
            .field("fragments", &self.fragments.keys().collect::<Vec<_>>())
            .field(
                "live",
                &self.live.as_ref().map(|b| format!("{} bytes", b.len())),
            )
            .field("owned_entries", &self.owned_entries)
            .field("owned_appended", &self.owned_appended)
            .finish()
    }
}

/// Produces the bytes of a generated file. Adapters implement it (docs/adapters.md). The trait
/// lives in `engine` until a `ports` crate exists (docs/architecture.md section 3); adapters
/// are in the CLI crate and do not depend on `engine` internals.
///
/// `generate` is a pure function of the operation and the verified inputs. It must not open
/// files. A refusal with `E_PLAN_STALE` means the inputs no longer match the plan (for example
/// a fragment appeared): apply reports the operation as blocked. Any other code is a failure.
pub trait Generator {
    fn supports(&self, op: &Operation) -> bool;
    fn generate(&self, op: &Operation, inputs: &GeneratorInputs) -> Result<Vec<u8>, Refusal>;

    /// Whether `op` is a sensitive co-owned merge this generator runs (`claude-settings/v1`):
    /// a `create_file`, `update_file` or `retire` operation. Apply admits exactly these
    /// sensitive operations and refuses every other one (docs/apply.md, Merge).
    fn merges(&self, _op: &Operation) -> bool {
        false
    }

    /// Merges the declared value into `inputs.live`, or for a `retire` operation removes the
    /// owned entries. A refusal with `E_CONFLICT` or `E_PLAN_STALE` blocks the operation and
    /// writes nothing. It never repeats a byte of the live file.
    fn merge(&self, op: &Operation, _inputs: &GeneratorInputs) -> Result<Merged, Refusal> {
        Err(Refusal::new(
            Code::GeneratorUnavailable,
            format!("no merge adapter for {}", op.adapter),
        ))
    }
}

/// The generator of a build with no adapters: every file operation is unsupported.
#[derive(Debug, Default)]
pub struct NoGenerator;

impl Generator for NoGenerator {
    fn supports(&self, _op: &Operation) -> bool {
        false
    }
    fn generate(&self, op: &Operation, _inputs: &GeneratorInputs) -> Result<Vec<u8>, Refusal> {
        Err(Refusal::new(
            Code::GeneratorUnavailable,
            format!("no generator for {}", op.adapter),
        ))
    }
}

/// Why the inputs could not be collected.
pub(crate) enum Gather {
    /// An input differs from the plan: the operation is blocked with `E_PLAN_STALE`.
    Stale(String),
    /// Anything else (a parent that is not safe, an I/O error).
    Failed(Refusal),
}

impl From<Refusal> for Gather {
    fn from(r: Refusal) -> Self {
        Gather::Failed(r)
    }
}

fn stale(detail: impl Into<String>) -> Gather {
    Gather::Stale(detail.into())
}

fn invalid(detail: impl Into<String>) -> Gather {
    Gather::Failed(Refusal::new(Code::PlanInvalid, detail))
}

/// Total bytes of the fragments one assemble operation may read. It equals the output cap, so
/// the fragments alone can never make a larger file than apply would publish.
pub const MAX_FRAGMENT_BYTES: u64 = MAX_HASHED_BYTES;

/// The bytes still open to an assemble operation's fragments. It is checked before each read.
struct Budget(u64);

impl Budget {
    fn read(&mut self, sys: &Sys, dir: &Dir, name: &str, path: &str) -> Result<Vec<u8>, Gather> {
        let whole = self.0 == MAX_FRAGMENT_BYTES;
        match dir.read_input(sys, name, self.0)? {
            Got::Bytes(b) => {
                self.0 -= u64::try_from(b.len()).unwrap_or(u64::MAX);
                Ok(b)
            }
            Got::Absent => Err(stale(format!(
                "fragment {path} changed since the plan: it is gone, not a plain file of yours or linked twice"
            ))),
            // Nothing was consumed yet: this one file is over the cap, as the planner found.
            Got::TooBig if whole => Err(stale(format!(
                "fragment {path} changed since the plan: it is over 16 MiB"
            ))),
            Got::TooBig => Err(too_much()),
        }
    }

    /// Takes bytes that were read elsewhere (a link producer's source file).
    fn take(&mut self, n: usize) -> Result<(), Gather> {
        self.0 = self
            .0
            .checked_sub(u64::try_from(n).unwrap_or(u64::MAX))
            .ok_or_else(too_much)?;
        Ok(())
    }
}

fn too_much() -> Gather {
    Gather::Failed(Refusal::new(
        Code::LimitExceeded,
        "the fragments together exceed 16 MiB",
    ))
}

/// Reads one source file of `owner` through the opened source root and compares it with the
/// `source:` precondition the plan recorded for it. The message names the path only.
fn read_source(sys: &Sys, root: &Dir, owner: &Operation, path: &str) -> Result<Vec<u8>, Gather> {
    let want = owner
        .preconditions
        .iter()
        .find(|p| p.subject == format!("source:{path}"))
        .map(|p| p.state.as_str());
    let bytes = match root.read_input_below(sys, path, MAX_HASHED_BYTES)? {
        Got::Bytes(b) => b,
        _ => {
            return Err(stale(format!(
                "source {path} changed since the plan: it is gone, not a plain file of yours, linked twice or over 16 MiB"
            )));
        }
    };
    if want != Some(format!("sha256:{}", sha256_hex(&bytes)).as_str()) {
        return Err(stale(format!("source {path} changed since the plan")));
    }
    Ok(bytes)
}

/// Reads and checks the inputs of `op`. `fragment_dir` is the opened fragment directory (below
/// HOME, parents verified by the caller), or `None` when the directory does not exist.
pub(crate) fn gather(
    plan: &Plan,
    op: &Operation,
    sys: &Sys,
    fragment_dir: Option<&Dir>,
    salt: Option<&[u8]>,
) -> Result<GeneratorInputs, Gather> {
    let mut out = GeneratorInputs::default();
    // A render has no file input. Nothing is read, so the source root is not opened either.
    if op.inputs.is_empty() && op.fragments.is_empty() && op.fragment_dir.is_none() {
        return Ok(out);
    }
    let [source] = plan.sources.as_slice() else {
        return Err(invalid(
            "a plan with generated files needs exactly one source",
        ));
    };
    let root = Dir::open_plain(Path::new(&source.root))?;
    for input in &op.inputs {
        let bytes = read_source(sys, &root, op, &input.path)?;
        out.sources.insert(input.path.clone(), bytes);
    }
    let Some(dir_rel) = op.fragment_dir.as_deref() else {
        if !op.fragments.is_empty() {
            return Err(invalid(format!(
                "{} lists fragments but no directory",
                op.id
            )));
        }
        return Ok(out);
    };
    let Some(dir) = fragment_dir else {
        // The directory does not exist. That is the plan's picture only when it lists nothing.
        if !op.fragments.is_empty() {
            return Err(stale(format!(
                "{dir_rel} changed since the plan: it is gone"
            )));
        }
        out.listing = Some(Vec::new());
        return Ok(out);
    };
    // The listing is cut like the planner's. A cut listing is not the plan's picture.
    let listing = dir.list_upto(MAX_LISTING)?;
    if listing.odd > 0 || listing.names.len() > MAX_LISTING {
        return Err(stale(format!(
            "{dir_rel} changed since the plan: it holds a name that is not UTF-8 or too many entries"
        )));
    }
    out.listing = Some(listing.names);
    let mut budget = Budget(MAX_FRAGMENT_BYTES);
    for f in &op.fragments {
        let path = format!("{dir_rel}/{}", f.name);
        let bytes = match f.origin.strip_prefix("selected:") {
            Some(resource) => {
                let producer = plan
                    .operations
                    .iter()
                    .find(|p| {
                        p.resource == resource
                            && p.path.as_deref().is_some_and(|pp| fold::eq(pp, &path))
                    })
                    .ok_or_else(|| invalid(format!("no operation produces fragment {path}")))?;
                selected(sys, &root, dir, producer, &mut budget, &f.name, &path)?
            }
            None => {
                let bytes = budget.read(sys, dir, &f.name, &path)?;
                // A fragment of a sensitive merge has no plain hash in the plan: it can hold
                // credentials. Its salted content token is part of the approved plan, which
                // apply plans again under the lock and compares before it writes.
                if op.class != Class::Sensitive {
                    let want = f.sha256.as_deref().ok_or_else(|| {
                        invalid(format!("foreign fragment {path} has no recorded hash"))
                    })?;
                    if sha256_hex(&bytes) != want {
                        return Err(stale(format!("fragment {path} changed since the plan")));
                    }
                }
                bytes
            }
        };
        out.fragments.insert(f.name.clone(), bytes);
    }
    // The fragments of a sensitive merge are tracked by a salted content token in the plan. The
    // re-plan under the lock compared it once; compare again with the bytes read now, so a
    // fragment swapped between that read and this one is stale too.
    let tokens = op
        .preconditions
        .iter()
        .find(|p| p.subject == format!("fragments:{dir_rel}"))
        .and_then(|p| p.state.split_once(" content=[").map(|(_, t)| t.to_string()));
    if let (Some(salt), Some(want), true) = (salt, tokens, op.class == Class::Sensitive) {
        let got: Vec<String> = out
            .fragments
            .iter()
            .map(|(n, b)| format!("{n}:{}", fragment_token(salt, n, b)))
            .collect();
        if got.join(",") != want.strip_suffix(']').unwrap_or(&want) {
            return Err(stale(format!(
                "fragment content in {dir_rel} changed since the plan"
            )));
        }
    }
    Ok(out)
}

/// The content of a fragment another selected resource produces. A generated producer wrote a
/// regular file whose hash is its planned output. A link producer wrote a link to its source
/// file: the link must be the planned one, and the content is that source file, read and
/// compared with the plan like any other input.
fn selected(
    sys: &Sys,
    root: &Dir,
    dir: &Dir,
    producer: &Operation,
    budget: &mut Budget,
    name: &str,
    path: &str,
) -> Result<Vec<u8>, Gather> {
    if let Some(want) = &producer.output_sha256 {
        let bytes = budget.read(sys, dir, name, path)?;
        if &sha256_hex(&bytes) != want {
            return Err(stale(format!("fragment {path} changed since the plan")));
        }
        return Ok(bytes);
    }
    let (Some(link), Some(input)) = (&producer.link, producer.inputs.first()) else {
        return Err(Gather::Failed(Refusal::new(
            Code::GeneratorUnavailable,
            format!("fragment {path} is neither generated nor a link to a source file"),
        )));
    };
    match dir.peek(sys, name)? {
        Peek::Link { target, .. } if &target == link => {}
        _ => {
            return Err(stale(format!(
                "fragment {path} changed since the plan: it is not the link the plan made"
            )));
        }
    }
    let bytes = read_source(sys, root, producer, &input.path)?;
    budget.take(bytes.len())?;
    Ok(bytes)
}

/// The live tree below one directory source: the entries the planner digests.
struct Tree {
    entries: Vec<TreeEntry>,
    bytes: u64,
}

fn tree_stale(path: &str) -> Gather {
    stale(format!(
        "source {path} changed since the plan: a directory entry is gone, special, not UTF-8 or over a limit"
    ))
}

/// An entry that could not be examined is an I/O fault, not a change of the source.
fn unreadable(path: &str, why: &str) -> Gather {
    Gather::Failed(Refusal::new(
        Code::IoFailed,
        format!("source {path} cannot be examined: {why}"),
    ))
}

/// Lists `dir` without following a link and records what the planner records for a directory
/// source: a directory, a file with its hash, or a link with its target text.
fn walk(
    sys: &Sys,
    dir: &Dir,
    (prefix, shown): (&str, &str),
    tree: &mut Tree,
) -> Result<(), Gather> {
    let room = MAX_SOURCE_ENTRIES.saturating_sub(tree.entries.len());
    let listing = dir.list_upto(room)?;
    if listing.odd > 0 || listing.names.len() > room {
        return Err(tree_stale(shown));
    }
    for name in listing.names {
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let kind = match dir.peek(sys, &name)? {
            Peek::Dir { .. } => {
                let child = dir.peek_dir(&name)?.ok_or_else(|| tree_stale(shown))?;
                tree.entries.push(TreeEntry {
                    path: rel.clone(),
                    kind: EntryKind::Dir,
                });
                walk(sys, &child, (&rel, shown), tree)?;
                continue;
            }
            Peek::File { .. } => {
                let bytes = dir
                    .read_plain(&name, MAX_SOURCE_FILE_BYTES)?
                    .ok_or_else(|| tree_stale(shown))?;
                tree.bytes += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                if tree.bytes > MAX_SOURCE_TOTAL_BYTES {
                    return Err(tree_stale(shown));
                }
                EntryKind::File {
                    exec: false,
                    sha256: sha256_hex(&bytes),
                }
            }
            Peek::Link { target, .. } => EntryKind::Symlink { target },
            Peek::Unknown(why) => return Err(unreadable(shown, &why)),
            _ => return Err(tree_stale(shown)),
        };
        tree.entries.push(TreeEntry { path: rel, kind });
    }
    Ok(())
}

/// Reads the source of a link operation again, immediately before the operation is journaled.
/// The source root is the trusted start; every component below it is opened without following a
/// link. The path must still be a regular file or a directory, and its content hash (file) or
/// tree digest (directory) must equal the plan's `source:` precondition. A source with no
/// recorded identity (a secret value) is checked for presence and type only.
///
/// shortcut: a directory source with entries that `[sources].exclude` left out is checked for
/// presence and type only, because the plan does not record the exclude patterns; revisit when
/// the plan carries the scope.
pub(crate) fn link_source(sys: &Sys, plan: &Plan, op: &Operation) -> Result<(), Gather> {
    let (Some(input), Some(link)) = (op.inputs.first(), op.link.as_deref()) else {
        return Ok(());
    };
    let source = plan
        .sources
        .iter()
        .find(|s| {
            link.strip_prefix(s.root.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
        })
        .ok_or_else(|| invalid(format!("{}: the link target lies in no source", op.id)))?;
    let path = input.path.as_str();
    let gone = || {
        stale(format!(
            "source {path} changed since the plan: it is gone, a parent is a link, or it is not a file or directory"
        ))
    };
    let want = op
        .preconditions
        .iter()
        .find(|p| p.subject == format!("source:{path}"))
        .and_then(|p| p.state.strip_prefix("sha256:"));
    let mut cur = Dir::open_plain(Path::new(&source.root))?;
    let mut parts: Vec<&str> = path.split('/').collect();
    let name = parts.pop().ok_or_else(gone)?;
    for part in parts {
        cur = cur.peek_dir(part)?.ok_or_else(gone)?;
    }
    match cur.peek(sys, name)? {
        Peek::File { .. } => {
            let bytes = cur.read_plain(name, MAX_HASHED_BYTES)?.ok_or_else(gone)?;
            if want.is_some_and(|h| h != sha256_hex(&bytes)) {
                return Err(stale(format!("source {path} changed since the plan")));
            }
        }
        Peek::Dir { .. } => {
            let dir = cur.peek_dir(name)?.ok_or_else(gone)?;
            if let (Some(want), None) = (want, op.excluded) {
                let mut tree = Tree {
                    entries: Vec::new(),
                    bytes: 0,
                };
                walk(sys, &dir, ("", path), &mut tree)?;
                if want != tree_digest(tree.entries) {
                    return Err(stale(format!("source {path} changed since the plan")));
                }
            }
        }
        Peek::Unknown(why) => return Err(unreadable(path, &why)),
        _ => return Err(gone()),
    }
    Ok(())
}
