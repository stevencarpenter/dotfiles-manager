//! The generator seam: how apply obtains the bytes of a generated file (docs/adapters.md).
//!
//! An adapter is a [`Generator`]. It never opens a file. Apply reads every input the plan
//! recorded, descriptor-relatively and without following links, compares each with the plan,
//! and hands the verified bytes to the generator in [`GeneratorInputs`]. The same function
//! serves `render` (no inputs beyond the resource value) and `assemble` (a body and the
//! fragments the planner listed).

use crate::apply::MAX_FILE;
use crate::fsys::{Dir, Peek, Sys};
use dfm_model::digest::sha256_hex;
use dfm_model::fold;
use dfm_model::plan::{Operation, Plan};
use dfm_model::{Code, Refusal};
use std::collections::BTreeMap;
use std::path::Path;

/// The verified live input of one generated file.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
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

/// Reads one source file of `owner` through the opened source root and compares it with the
/// `source:` precondition the plan recorded for it.
fn read_source(root: &Dir, owner: &Operation, path: &str) -> Result<Vec<u8>, Gather> {
    let want = owner
        .preconditions
        .iter()
        .find(|p| p.subject == format!("source:{path}"))
        .map(|p| p.state.as_str());
    let bytes = root
        .read_below(path, MAX_FILE)?
        .ok_or_else(|| stale(format!("source {path} is gone or not a plain file")))?;
    let have = format!("sha256:{}", sha256_hex(&bytes));
    if want != Some(have.as_str()) {
        return Err(stale(format!(
            "source {path} was {} and is now {have}",
            want.unwrap_or("not recorded")
        )));
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
        let bytes = read_source(&root, op, &input.path)?;
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
        if let Some(f) = op.fragments.first() {
            return Err(stale(format!(
                "{dir_rel} is gone; the plan lists {}",
                f.name
            )));
        }
        out.listing = Some(Vec::new());
        return Ok(out);
    };
    let listing = dir.list()?;
    if listing.odd > 0 {
        return Err(stale(format!(
            "{dir_rel} now holds a name that is not UTF-8"
        )));
    }
    out.listing = Some(listing.names);
    for f in &op.fragments {
        let path = format!("{dir_rel}/{}", f.name);
        let bytes = match f.origin.strip_prefix("selected:") {
            Some(resource) => {
                let producer = plan
                    .operations
                    .iter()
                    .find(|p| {
                        p.resource == resource
                            && p.path.as_deref().is_some_and(|pp| fold(pp) == fold(&path))
                    })
                    .ok_or_else(|| invalid(format!("no operation produces fragment {path}")))?;
                selected(&root, dir, sys, producer, &f.name, &path)?
            }
            None => {
                let want = f.sha256.as_deref().ok_or_else(|| {
                    invalid(format!("foreign fragment {path} has no recorded hash"))
                })?;
                let bytes = plain(dir, &f.name, &path)?;
                let have = sha256_hex(&bytes);
                if have != want {
                    return Err(stale(format!(
                        "fragment {path} was {want} and is now {have}"
                    )));
                }
                bytes
            }
        };
        out.fragments.insert(f.name.clone(), bytes);
    }
    Ok(out)
}

fn plain(dir: &Dir, name: &str, path: &str) -> Result<Vec<u8>, Gather> {
    dir.read_plain(name, MAX_FILE)?.ok_or_else(|| {
        stale(format!(
            "fragment {path} is gone, is not a plain file or is over 16 MiB"
        ))
    })
}

/// The content of a fragment another selected resource produces. A generated producer wrote a
/// regular file whose hash is its planned output. A link producer wrote a link to its source
/// file: the link must be the planned one, and the content is that source file, read and
/// compared with the plan like any other input.
fn selected(
    root: &Dir,
    dir: &Dir,
    sys: &Sys,
    producer: &Operation,
    name: &str,
    path: &str,
) -> Result<Vec<u8>, Gather> {
    if let Some(want) = &producer.output_sha256 {
        let bytes = plain(dir, name, path)?;
        let have = sha256_hex(&bytes);
        if &have != want {
            return Err(stale(format!(
                "fragment {path} was {want} and is now {have}"
            )));
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
                "fragment {path} is not the link the plan made"
            )));
        }
    }
    read_source(root, producer, &input.path)
}
