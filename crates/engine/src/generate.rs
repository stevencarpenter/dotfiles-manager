//! The generator seam: how apply obtains the bytes of a generated file (docs/adapters.md).
//!
//! An adapter is a [`Generator`]. It never opens a file. Apply reads every input the plan
//! recorded, descriptor-relatively and without following links, compares each with the plan,
//! and hands the verified bytes to the generator in [`GeneratorInputs`]. The same function
//! serves `render` (no inputs beyond the resource value) and `assemble` (a body and the
//! fragments the planner listed).

use crate::fsys::{Dir, Got, Listing, Peek, Sys};
use dfm_model::digest::{EntryKind, TreeEntry, sha256_hex, tree_digest};
use dfm_model::fold;
use dfm_model::merge::{Merged, fragment_token};
use dfm_model::plan::{
    MAX_HASHED_BYTES, MAX_LISTING, MAX_SOURCE_ENTRIES, MAX_SOURCE_FILE_BYTES,
    MAX_SOURCE_TOTAL_BYTES, Operation, PackageState, Plan, ServiceState, file_fingerprint,
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

    /// Whether `op` is a secret output this generator renders (`op-template/v1`): a sensitive
    /// `create_file` or `update_file` whose content a provider produces at apply time
    /// (docs/secrets.md). Apply admits it beside the merge and refuses every other sensitive
    /// operation.
    fn renders(&self, _op: &Operation) -> bool {
        false
    }

    /// Renders `template` through the provider and returns the bytes to publish. Unlike
    /// `generate`, this starts a child process. `E_CREDENTIAL_UNAVAILABLE` means the provider
    /// has no session and nothing ran; `E_RENDER_FAILED` means it ran and failed. The bytes are
    /// a secret: no caller may print, hash unsalted, or persist them outside the destination.
    fn render(&self, op: &Operation, _template: &[u8]) -> Result<Vec<u8>, Refusal> {
        Err(Refusal::new(
            Code::GeneratorUnavailable,
            format!("no render adapter for {}", op.adapter),
        ))
    }

    /// Whether `op` is a package install or upgrade this generator runs (docs/packages.md,
    /// docs/toolchains.md): an `install` or `upgrade` operation of a provider this build has.
    fn installs(&self, _op: &Operation) -> bool {
        false
    }

    /// Asks the declared provider whether the package is installed (a read-only listing).
    fn probe(&self, op: &Operation) -> Result<PackageState, Refusal> {
        Err(Refusal::new(
            Code::GeneratorUnavailable,
            format!("no package provider for {}", op.adapter),
        ))
    }

    /// Installs the package through the declared provider, or for an `upgrade` operation asks
    /// the provider for its newest eligible version. The caller probes again afterwards.
    fn install(&self, op: &Operation) -> Result<(), Refusal> {
        Err(Refusal::new(
            Code::GeneratorUnavailable,
            format!("no package provider for {}", op.adapter),
        ))
    }

    /// Whether `op` is a service start or restart this generator runs (docs/lifecycle.md): a
    /// `start` or `restart` operation of a launcher this build has.
    fn reloads(&self, _op: &Operation) -> bool {
        false
    }

    /// Asks the launcher what it holds for the service (a read-only report).
    fn service_state(&self, op: &Operation) -> Result<ServiceState, Refusal> {
        Err(Refusal::new(
            Code::GeneratorUnavailable,
            format!("no service launcher for {}", op.adapter),
        ))
    }

    /// Starts or restarts the service through its launcher, as `op.action` says, and waits a
    /// bounded time for the launcher to report completion. The caller probes again afterwards.
    fn service_act(&self, op: &Operation) -> Result<Completion, Refusal> {
        Err(Refusal::new(
            Code::GeneratorUnavailable,
            format!("no service launcher for {}", op.adapter),
        ))
    }
}

/// How a launcher answered a start or restart (architecture section 11, `UpdateProvider`:
/// queued is `pending`, only completed can become `updated`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Completion {
    /// The launcher finished the request. The caller's postcondition probe decides the outcome.
    Completed,
    /// The launcher accepted the request and did not report completion within the bound. The
    /// operation is `pending`; the detail says what was observed.
    Queued(String),
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
                    if file_fingerprint(&bytes) != want {
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

/// Entries the containment walk of a directory source with excluded content may examine in all.
/// The plan never lists inside an excluded directory, so the walk has to cover a large virtual
/// environment or `node_modules`. It matches the count the source loader stops at when it
/// reports the biggest directories.
const MAX_LINK_WALK_ENTRIES: usize = 1_000_000;
/// Deepest directory the walk enters. It holds one descriptor per level.
const MAX_LINK_WALK_DEPTH: usize = 64;

/// The refusal for a listing the walk cannot trust: a name that is not UTF-8, or more entries
/// than the walk has left (the listing stops one past the budget, so it may be cut).
fn listing_refusal(listing: &Listing, budget: usize, shown: &str) -> Option<Refusal> {
    if listing.odd > 0 {
        return Some(Refusal::new(
            Code::SourceUntrusted,
            format!("source {shown} holds an entry whose name is not UTF-8"),
        ));
    }
    (listing.names.len() > budget).then(|| {
        Refusal::new(
            Code::LimitExceeded,
            format!("source {shown} holds more than {MAX_LINK_WALK_ENTRIES} entries"),
        )
    })
}

/// A directory source with excluded content is not hashed, but a link inside it is still
/// published. Every link below `dir` must stay inside the source directory (see
/// [`dfm_model::resolve::target_stays_inside`]); `depth` is the number of levels between that directory and `dir`.
/// The check fails closed: a listing that was cut, an entry that is not UTF-8, a directory or
/// entry that cannot be examined, and a walk over the entry or depth limits all refuse.
fn links_stay_inside(
    sys: &Sys,
    dir: &Dir,
    (depth, shown): (usize, &str),
    budget: &mut usize,
) -> Result<(), Gather> {
    if depth > MAX_LINK_WALK_DEPTH {
        return Err(Gather::Failed(Refusal::new(
            Code::LimitExceeded,
            format!("source {shown} has directories nested more than {MAX_LINK_WALK_DEPTH} deep"),
        )));
    }
    let listing = dir.list_upto(*budget)?;
    listing_refusal(&listing, *budget, shown).map_or(Ok(()), |r| Err(Gather::Failed(r)))?;
    *budget -= listing.names.len();
    for name in listing.names {
        match dir.peek(sys, &name)? {
            Peek::Link { target, .. }
                if !dfm_model::resolve::target_stays_inside(&target, depth) =>
            {
                return Err(Gather::Failed(Refusal::new(
                    Code::SourceEscape,
                    format!("source link {name} in {shown} leaves the source directory"),
                )));
            }
            Peek::Dir { .. } => {
                let child = dir.peek_dir(&name)?.ok_or_else(|| tree_stale(shown))?;
                links_stay_inside(sys, &child, (depth + 1, shown), budget)?;
            }
            Peek::Missing => return Err(tree_stale(shown)),
            Peek::Unknown(why) => return Err(unreadable(shown, &why)),
            Peek::File { .. } | Peek::Link { .. } | Peek::Other { .. } => {}
        }
    }
    Ok(())
}

/// Reads the source of a link operation again, immediately before the operation is journaled.
/// The source root is the trusted start; every component below it is opened without following a
/// link. The path must still be a regular file or a directory, and its content hash (file) or
/// tree digest (directory) must equal the plan's `source:` precondition. A source with no
/// recorded identity (a secret value) is checked for presence and type only.
///
/// shortcut: a directory source with entries that `[sources].exclude` left out is not compared
/// with its digest, because the plan does not record the exclude patterns; its links are only
/// checked to stay inside the source directory. Revisit when the plan carries the scope.
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
            if op.excluded.is_some() {
                links_stay_inside(sys, &dir, (0, path), &mut { MAX_LINK_WALK_ENTRIES })?;
            } else if let Some(want) = want {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::PathBuf;

    /// A scratch directory below the system temporary directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Scratch {
            let p = std::env::temp_dir().join(format!("dfm-gen-{label}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Scratch(p)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            // A test may leave a directory unreadable on purpose.
            let _ = fs::set_permissions(self.0.join("d/locked"), fs::Permissions::from_mode(0o755));
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The outcome of the walk over `<scratch>/d` with `budget` entries: `None` when it passes,
    /// else the code of the refusal.
    fn walk_code(s: &Scratch, budget: usize) -> Option<Code> {
        let sys = Sys { uid: 0 };
        let dir = Dir::open_plain(&s.0.join("d")).unwrap();
        match links_stay_inside(&sys, &dir, (0, "d"), &mut { budget }) {
            Ok(()) => None,
            Err(Gather::Failed(r)) => Some(r.code),
            Err(Gather::Stale(why)) => panic!("stale: {why}"),
        }
    }

    #[test]
    fn a_link_two_levels_down_may_climb_two_levels_and_not_three() {
        let s = Scratch::new("climb");
        fs::create_dir_all(s.0.join("d/a/b")).unwrap();
        symlink("../../x", s.0.join("d/a/b/ok")).unwrap();
        assert_eq!(walk_code(&s, 100), None);
        symlink("../../../x", s.0.join("d/a/b/out")).unwrap();
        assert_eq!(walk_code(&s, 100), Some(Code::SourceEscape));
    }

    #[test]
    fn a_chain_of_links_cannot_climb_past_the_directory() {
        // l1 -> .. (the directory), and then l3 -> l1/../x would leave it.
        let s = Scratch::new("chain");
        fs::create_dir_all(s.0.join("d/a")).unwrap();
        symlink("..", s.0.join("d/a/l1")).unwrap();
        assert_eq!(walk_code(&s, 100), None);
        symlink("l1/../../x", s.0.join("d/a/l3")).unwrap();
        assert_eq!(walk_code(&s, 100), Some(Code::SourceEscape));
    }

    #[test]
    fn a_listing_that_was_cut_is_a_refusal_not_a_pass() {
        let s = Scratch::new("cut");
        fs::create_dir_all(s.0.join("d")).unwrap();
        for n in ["a", "b", "c"] {
            fs::write(s.0.join("d").join(n), "x").unwrap();
        }
        symlink("/etc", s.0.join("d/z")).unwrap();
        assert_eq!(walk_code(&s, 3), Some(Code::LimitExceeded));
        // With room for every entry the link is found.
        assert_eq!(walk_code(&s, 4), Some(Code::SourceEscape));
        // The budget is shared over the whole walk, so a later directory cannot be skipped.
        fs::remove_file(s.0.join("d/z")).unwrap();
        fs::create_dir_all(s.0.join("d/e")).unwrap();
        symlink("/etc", s.0.join("d/e/z")).unwrap();
        assert_eq!(walk_code(&s, 4), Some(Code::LimitExceeded));
        assert_eq!(walk_code(&s, 5), Some(Code::SourceEscape));
    }

    #[test]
    fn a_name_that_is_not_utf8_is_a_refusal() {
        let s = Scratch::new("odd");
        fs::create_dir_all(s.0.join("d")).unwrap();
        let odd = std::ffi::OsStr::from_bytes(b"bad\xff");
        // APFS refuses a name that is not UTF-8, so there is nothing to walk on that volume.
        if symlink("/etc", s.0.join("d").join(odd)).is_ok() {
            assert_eq!(walk_code(&s, 100), Some(Code::SourceUntrusted));
        }
    }

    #[test]
    fn a_listing_with_an_odd_name_or_too_many_entries_is_refused_on_every_volume() {
        // The same decision as the walk makes, on a listing built by hand, so it does not need a
        // volume that accepts a name which is not UTF-8.
        let names = |n: usize| (0..n).map(|i| i.to_string()).collect::<Vec<_>>();
        let listing = |names: Vec<String>, odd| Listing { names, odd };
        let code = |l: Listing, budget| listing_refusal(&l, budget, "d").map(|r| r.code);
        assert_eq!(code(listing(names(1), 1), 10), Some(Code::SourceUntrusted));
        assert_eq!(code(listing(names(4), 0), 3), Some(Code::LimitExceeded));
        assert_eq!(code(listing(names(3), 0), 3), None);
        assert_eq!(code(listing(Vec::new(), 0), 0), None);
    }

    #[test]
    fn a_directory_that_cannot_be_opened_is_a_refusal() {
        let s = Scratch::new("locked");
        fs::create_dir_all(s.0.join("d/locked")).unwrap();
        symlink("/etc", s.0.join("d/locked/esc")).unwrap();
        fs::set_permissions(s.0.join("d/locked"), fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads any directory, so the mode proves nothing there.
        if fs::metadata(&s.0).unwrap().uid() != 0 {
            assert!(walk_code(&s, 100).is_some());
        }
    }

    #[test]
    fn a_walk_deeper_than_the_limit_is_a_refusal() {
        let s = Scratch::new("deep");
        let deep = (0..=MAX_LINK_WALK_DEPTH + 1)
            .map(|_| "n")
            .collect::<Vec<_>>()
            .join("/");
        fs::create_dir_all(s.0.join("d").join(deep)).unwrap();
        assert_eq!(walk_code(&s, 1000), Some(Code::LimitExceeded));
    }
}
