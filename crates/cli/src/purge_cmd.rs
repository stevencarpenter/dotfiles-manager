//! `purge`: end a trial by removing what it added (docs/purge.md). The command uninstalls the
//! package the trial installed and the Homebrew dependencies that install added, removes every
//! recorded configuration and data path that was absent before the trial, then removes the
//! trial record. Anything that existed before the trial is kept. The run needs `--yes-plan`
//! with the digest of the preview, as `trial` and `adopt` do. A script trial (DFM-82) also
//! removes what its observation window saw `created` below HOME, and prints the rest as
//! `retained:`.

use crate::adapters::package::{self, HOMEBREW, MISE};
use crate::adopt_cmd::overlaps;
use crate::apply_cmd::{is_digest, label, stale};
use crate::trial_cmd::window::{self, Watch};
use crate::{Ctx, cli, paint, read_existing_binding, state_cmd, usage};
use dfm_engine::fsys::{Dir, Peek, Sys};
use dfm_engine::{Runner, Store, now_ns, signal};
use dfm_model::digest::sha256_hex;
use dfm_model::display::{bounded_visible, visible};
use dfm_model::fold::{eq as same_path, fold};
use dfm_model::plan::{Binary, PackageState, file_fingerprint};
use dfm_model::resolve::{Table, Value};
use dfm_model::state::{
    Approval, BaselinePath, BinaryId, Class, InodeFence, JournalEntry, MAX_BYTES, OperationRecord,
    Phase, ProfileRef, Receipt, Recovery, RecoveryOutput, RunResult, SCHEMA, Stamp, Status,
    TrialRecord, TrialScript, WindowEntry, count_statuses, rfc3339,
};
use dfm_model::{Code, Exit, Refusal};
use serde_json::json;
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

/// The most entries a removed directory may hold. A larger tree is kept: its preview could not
/// show what the approval covers.
const MAX_ENTRIES: usize = 100_000;
/// The most bytes of relative paths a tree fingerprint holds in memory; a larger tree is kept.
const MAX_TREE_LIST_BYTES: usize = 16 << 20;
/// Why no directory of a truncated window is removed as a tree.
const TRUNCATED_WHY: &str = "the window was truncated; its contents were not all recorded";
/// Why no directory is removed as a tree when a window entry's name is inexact: a kept link with
/// such a name cannot be read back, so where it points is unknown.
const INEXACT_WHY: &str =
    "a window entry has a name that cannot be shown exactly; links in it cannot be followed";

#[derive(Debug)]
pub struct Args {
    /// The trial id, or the tool name of a trial with the default id `trial-<tool>`.
    pub trial: String,
    pub preview: bool,
    pub yes_plan: Option<String>,
}

/// `purge <trial-id> [--preview] [-y|--yes-plan <digest>]`.
pub fn parse_args(m: &clap::ArgMatches) -> Result<Args, Refusal> {
    let (trial, yes, preview) = (
        cli::one(m, "trial-id"),
        cli::one(m, "yes-plan"),
        cli::flag(m, "preview"),
    );
    if preview && yes.is_some() {
        return Err(usage("--preview and --yes-plan cannot be used together"));
    }
    if let Some(d) = &yes
        && !is_digest(d)
    {
        return Err(usage(
            "--yes-plan takes the 64 hex characters of a plan digest",
        ));
    }
    Ok(Args {
        trial: trial.ok_or_else(|| usage("purge needs a trial id"))?,
        preview,
        yes_plan: yes,
    })
}

/// What purge does with one item: change it, find nothing to do, or keep it for a reason.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Do,
    Absent,
    Keep(String),
}

/// A recorded path as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Now {
    Absent,
    File(String),
    /// Entry count, bytes and the fingerprint of the tree (relative path, kind, size).
    Dir(usize, u64, String),
    /// A link a script trial created, with its target text. Only a script trial removes one.
    Link(String),
    Other(String),
}

#[derive(Debug, Clone)]
struct PathItem {
    path: String,
    now: Now,
    step: Step,
    /// The inode fence a script trial's candidate must still pass when it is removed; `None`
    /// for a package trial, which has no fence.
    fence: Option<InodeFence>,
}

/// What the preview shows and the digest covers.
struct Decision {
    trial_id: String,
    tool: String,
    /// The provider and the package, when the record names one, with the step for it.
    package: Option<(String, String, Step)>,
    deps: Vec<(String, Step)>,
    paths: Vec<PathItem>,
    /// What a script trial's window saw and purge never removes, one line each.
    retained: Vec<String>,
    unknown: Vec<String>,
    /// The script URL of a script trial, which installs no package.
    script: Option<String>,
    /// A script trial whose run did not finish: its candidates come from `before` and the
    /// live state.
    interrupted: bool,
    binary: Binary,
    stamp: Stamp,
}

fn step_json(s: &Step) -> serde_json::Value {
    match s {
        Step::Do => json!("do"),
        Step::Absent => json!("absent"),
        Step::Keep(why) => json!({ "keep": why }),
    }
}

impl Decision {
    fn digest(&self) -> Result<String, Refusal> {
        let now = |n: &Now| match n {
            Now::Absent => json!("absent"),
            Now::File(h) => json!({ "file": h }),
            Now::Dir(n, b, f) => json!({ "dir": f, "entries": n, "bytes": b }),
            Now::Link(t) => json!({ "link": t }),
            Now::Other(w) => json!({ "other": w }),
        };
        let doc = json!({
            "trial_id": self.trial_id,
            "tool": self.tool,
            "package": self.package.as_ref().map(|(a, n, s)| json!({"adapter": a, "name": n, "step": step_json(s)})),
            "deps": self.deps.iter().map(|(n, s)| json!({"name": n, "step": step_json(s)})).collect::<Vec<_>>(),
            "paths": self.paths.iter().map(|p| json!({"path": p.path, "now": now(&p.now), "step": step_json(&p.step)})).collect::<Vec<_>>(),
            "retained": self.retained,
            "unknown": self.unknown,
            "binary": {"version": self.binary.version, "sha256": self.binary.sha256},
            "binding": {
                "profile": self.stamp.profile,
                "machine_id": self.stamp.machine_id,
                "binding_digest": self.stamp.binding_digest,
            },
        });
        serde_json::to_vec(&doc)
            .map(|b| sha256_hex(&b))
            .map_err(|e| Refusal::new(Code::IoFailed, format!("cannot digest the purge: {e}")))
    }

    fn print(&self, out: &mut dyn Write, digest: &str) -> std::io::Result<()> {
        writeln!(out, "purge: {} tool={}", self.trial_id, self.tool)?;
        match (&self.package, &self.script) {
            (None, Some(url)) => writeln!(
                out,
                "package: none; the trial ran the script {}",
                visible(url)
            )?,
            (None, None) => writeln!(out, "package: none recorded; the trial installed nothing")?,
            (Some((a, n, s)), _) => line(out, &format!("uninstall {a} {}", visible(n)), s)?,
        }
        for (n, s) in &self.deps {
            line(
                out,
                &format!(
                    "uninstall dependency {HOMEBREW} {} (kept if an installed formula needs it)",
                    visible(n)
                ),
                s,
            )?;
        }
        for p in &self.paths {
            if let (Now::Link(t), Step::Keep(why)) = (&p.now, &p.step) {
                writeln!(
                    out,
                    "kept: {} -> {} ({})",
                    visible(&p.path),
                    visible(t),
                    visible(why)
                )?;
                continue;
            }
            let what = match &p.now {
                Now::Absent => String::new(),
                // The inode and ctime are for the digest, not for the reader.
                Now::File(h) if h.starts_with("size:") => {
                    format!(" file ({})", h.split(' ').next().unwrap_or(h))
                }
                Now::File(h) => format!(" file sha256:{h}"),
                Now::Dir(n, b, _) => format!(" directory ({n} entries, {b} bytes)"),
                Now::Link(t) => format!(" link -> {}", visible(t)),
                Now::Other(w) => format!(" {w}"),
            };
            line(out, &format!("remove {}{what}", visible(&p.path)), &p.step)?;
        }
        for r in &self.retained {
            writeln!(out, "retained: {r}")?;
        }
        if self.interrupted {
            writeln!(
                out,
                "note: the script run of this trial did not finish; each path created since its before observation is a candidate"
            )?;
        }
        writeln!(
            out,
            "retained: {} (not recorded by the trial, never cleaned)",
            self.unknown.join(" ")
        )?;
        writeln!(out, "plan digest: {digest}")
    }
}

fn line(out: &mut dyn Write, what: &str, s: &Step) -> std::io::Result<()> {
    match s {
        Step::Do => writeln!(out, "action: {what}"),
        Step::Absent => writeln!(out, "nothing: {what} (already absent)"),
        Step::Keep(why) => writeln!(out, "keep: {what}: {}", visible(why)),
    }
}

/// Whether two recorded names can share an install: brew treats `atuin` and
/// `homebrew/core/atuin` as one formula, and two mise trials of one tool (`node`, `node@22`,
/// `node@22.1`) can use the same installed version, so mise compares the tool alone.
fn same_package(adapter: &str, a: &str, b: &str) -> bool {
    match adapter {
        HOMEBREW => package::brewfile_names(a).0 == package::brewfile_names(b).0,
        MISE => package::tool_and_version(a).0 == package::tool_and_version(b).0,
        _ => a == b,
    }
}

fn package_value(name: &str) -> Value {
    let mut t = Table::new();
    t.insert("name".into(), Value::String(name.into()));
    // The adapters require a policy; purge never updates, so `hold` is the neutral one.
    t.insert("update_policy".into(), Value::String("hold".into()));
    Value::Table(t)
}

/// The probe answer as a step. A probe that cannot answer refuses the whole purge: removing the
/// record then would lose the only evidence of what the trial installed.
/// Only the provider's own listing is asked: the install-planning checks of `probe` (another
/// owner, the official name, the dependencies) do not answer "is it installed".
fn probe_step(runner: &Runner, adapter: &str, name: &str) -> Result<Step, Refusal> {
    match package::listed(runner, adapter, &package_value(name))? {
        PackageState::Installed(_) => Ok(Step::Do),
        PackageState::Missing(_) => Ok(Step::Absent),
        PackageState::Unknown(why) => Err(Refusal::new(
            Code::ProbeFailed,
            format!(
                "{adapter} cannot say whether {name} is installed ({why}); nothing was changed"
            ),
        )),
        s => Ok(Step::Keep(format!("{adapter} reports {}", s.state()))),
    }
}

/// Why `remove_tree` would refuse the directory `m`: it opens each directory only when this
/// user owns it and group and others cannot write it, and it can unlink inside it only when the
/// owner can write it.
fn unremovable(m: &fs::Metadata, uid: u32) -> Option<String> {
    let mode = m.mode() & 0o7777;
    if m.uid() != uid {
        Some("another user owns a directory in it".into())
    } else if mode & 0o022 != 0 {
        Some(format!(
            "a directory in it has mode {mode:o} (group or others can write)"
        ))
    } else if mode & 0o200 == 0 {
        Some(format!(
            "a directory in it has mode {mode:o} (the owner cannot write)"
        ))
    } else {
        None
    }
}

/// The tree below `dir`: entries, bytes and a fingerprint of every relative path, kind, size,
/// inode and ctime, without following a link. `Ok(Err(reason))` when the tree cannot be removed whole: more than
/// [`MAX_ENTRIES`] entries, a name that is not UTF-8, or a directory [`unremovable`] refuses.
fn tree(dir: &Path, uid: u32) -> std::io::Result<Result<(usize, u64, String), String>> {
    tree_capped(dir, uid, MAX_TREE_LIST_BYTES)
}

/// [`tree`] with the path-list budget `cap` (bytes): a tree whose listed paths need more is
/// refused, so deep trees with long names cannot exhaust memory.
fn tree_capped(
    dir: &Path,
    uid: u32,
    cap: usize,
) -> std::io::Result<Result<(usize, u64, String), String>> {
    if let Some(why) = unremovable(&fs::symlink_metadata(dir)?, uid) {
        return Ok(Err(why));
    }
    // The root's own inode and ctime: an empty directory swapped for another empty one, or
    // one whose entries changed and changed back, is then a different tree.
    let root = fs::symlink_metadata(dir)?;
    let mut lines = vec![format!(
        ".\td\t{}\t{}.{}",
        root.ino(),
        root.ctime(),
        root.ctime_nsec()
    )];
    let mut listed = lines[0].len();
    let mut bytes = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d)? {
            let p = e?.path();
            let m = fs::symlink_metadata(&p)?;
            let Some(rel) = p.strip_prefix(dir).ok().and_then(|r| r.to_str()) else {
                return Ok(Err("a name in it is not UTF-8".into()));
            };
            let kind = if m.is_dir() {
                if let Some(why) = unremovable(&m, uid) {
                    return Ok(Err(why));
                }
                stack.push(p.clone());
                "d"
            } else if m.is_file() {
                bytes += m.len();
                "f"
            } else if m.is_symlink() {
                "l"
            } else {
                "o"
            };
            // The inode and ctime catch a rewrite of the same length: every content write
            // sets ctime, and no user call can set it back.
            let line = format!(
                "{rel}\t{kind}\t{}\t{}\t{}.{}",
                m.len(),
                m.ino(),
                m.ctime(),
                m.ctime_nsec()
            );
            listed += line.len();
            lines.push(line);
            // The root line is not an entry.
            if lines.len() - 1 > MAX_ENTRIES {
                return Ok(Err(format!("more than {MAX_ENTRIES} entries")));
            }
            if listed > cap {
                return Ok(Err(format!(
                    "its paths take more than {} MiB to list",
                    cap >> 20
                )));
            }
        }
    }
    lines.sort();
    Ok(Ok((
        lines.len() - 1,
        bytes,
        sha256_hex(lines.join("\n").as_bytes()),
    )))
}

/// Given a created link's target text, why purge keeps the link, or `None` to remove it.
type LinkKeep<'a> = dyn Fn(&str) -> Option<String> + 'a;

/// A path another active script trial changed: its id, the path, and whether its window names
/// the path (else it is in the `before` of a run that did not finish).
type Seen = (String, String, bool);

/// One recorded path now, and what purge does with it.
/// `others` are the paths the other active trials recorded; one that overlaps the path keeps
/// it. `seen` are the paths the other active script trials changed: a window path that overlaps
/// the path keeps it, and a `before` path keeps it when at or below it. `link_keep` is set for a link a script
/// trial created: given the link's target text, it answers why purge keeps the link, or `None`
/// when purge may remove it. Without it a link is kept.
fn path_item(
    b: &BaselinePath,
    owned: &[String],
    others: &[(String, String)],
    seen: &[Seen],
    sys: &Sys,
    home: &Path,
    link_keep: Option<&LinkKeep>,
) -> PathItem {
    let item = |now, step| PathItem {
        path: b.path.clone(),
        now,
        step,
        fence: None,
    };
    // The parents as `remove_path` opens them: real directories this user owns, no link.
    let parents = Path::new(&b.path)
        .strip_prefix(home)
        .ok()
        .and_then(|r| r.parent())
        .and_then(|r| r.to_str())
        .map(|r| r.split('/').filter(|c| !c.is_empty()).collect::<Vec<_>>());
    let Some(parents) = parents else {
        return item(
            Now::Other("outside HOME".into()),
            Step::Keep("outside HOME".into()),
        );
    };
    match Dir::open_home(sys, home).and_then(|h| h.open_below_opt(sys, &parents)) {
        Ok(Some(_)) => {}
        Ok(None) => return item(Now::Absent, Step::Absent),
        Err(e) => {
            return item(
                Now::Other("unsafe parent".into()),
                Step::Keep(format!("a parent is unsafe ({})", e.detail)),
            );
        }
    }
    let m = match fs::symlink_metadata(&b.path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return item(Now::Absent, Step::Absent);
        }
        Err(e) => {
            return item(
                Now::Other(format!("unreadable ({})", e.kind())),
                Step::Keep("cannot be read".into()),
            );
        }
    };
    let now = if m.is_dir() {
        match tree(Path::new(&b.path), sys.uid) {
            Ok(Ok((n, bytes, f))) => Now::Dir(n, bytes, f),
            Ok(Err(why)) => {
                return item(Now::Other("directory".into()), Step::Keep(why));
            }
            Err(e) => {
                return item(
                    Now::Other("directory".into()),
                    Step::Keep(format!("cannot be listed ({})", e.kind())),
                );
            }
        }
    } else if m.is_file() && m.len() > MAX_BYTES as u64 {
        // A large file is named by its size; reading it whole only to hash it is not needed.
        Now::File(big_file(&m))
    } else if m.is_file() {
        match fs::read(&b.path) {
            Ok(bytes) => Now::File(file_fingerprint(&bytes)),
            Err(e) => {
                return item(
                    Now::Other("file".into()),
                    Step::Keep(format!("cannot be read ({})", e.kind())),
                );
            }
        }
    } else if let (true, Some(keep)) = (m.is_symlink(), link_keep) {
        match fs::read_link(&b.path) {
            Ok(t) => {
                // A lossy form can match a different target, so it is never removal authority.
                let Some(t) = t.to_str().map(String::from) else {
                    return item(
                        Now::Link(t.to_string_lossy().into_owned()),
                        Step::Keep("its target is not UTF-8, so it cannot be compared".into()),
                    );
                };
                if let Some(why) = keep(&t) {
                    return item(Now::Link(t), Step::Keep(why));
                }
                Now::Link(t)
            }
            Err(e) => {
                return item(
                    Now::Other("link".into()),
                    Step::Keep(format!("cannot be read ({})", e.kind())),
                );
            }
        }
    } else {
        return item(
            Now::Other("link or special file".into()),
            Step::Keep("a link or a special file is never removed".into()),
        );
    };
    let step = if b.existed {
        Step::Keep("existed before the trial".into())
    } else if owned.iter().any(|o| overlaps(o, &b.path)) {
        Step::Keep("a module owns a path at or below it".into())
    } else if let Some((id, _)) = others.iter().find(|(_, p)| overlaps(p, &b.path)) {
        Step::Keep(format!("trial {id} also records it"))
    } else if let Some((id, p, _)) = seen.iter().find(|(_, p, window)| {
        if *window {
            overlaps(p, &b.path)
        } else {
            at_or_below(p, &b.path)
        }
    }) {
        Step::Keep(format!("trial {id} observed {}", visible(p)))
    } else {
        Step::Do
    };
    item(now, step)
}

/// Whether `p` is `dir` or lies below it, after the case fold.
fn at_or_below(p: &str, dir: &str) -> bool {
    let (p, d) = (fold(p), fold(dir));
    p == d || p.starts_with(&format!("{d}/"))
}

/// `p` with `.` and `..` resolved by text alone, so a relative link target can be compared with
/// a directory path. Nothing on disk is read.
fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// Why purge keeps the created link `link` whose target text is now `target`, or `None` when it
/// may unlink it: the target is the `recorded` one, or resolves (on the file system) inside, or
/// to, one of `created_dirs`.
fn link_keep(
    link: &Path,
    recorded: Option<&str>,
    target: &str,
    created_dirs: &[&Path],
) -> Option<String> {
    let recorded = lossless(recorded);
    if recorded == Some(target) {
        return None;
    }
    if !vouching(link, target, created_dirs).is_empty() {
        return None;
    }
    Some(match recorded {
        None => "no target recorded".into(),
        Some(_) => "repointed since the trial".into(),
    })
}

/// `recorded` unless it holds U+FFFD, which a lossy conversion of a target that is not UTF-8
/// leaves: such a form can equal a different target, so it is never removal authority.
fn lossless(recorded: Option<&str>) -> Option<&str> {
    // U+FFFD stands for bytes that were not UTF-8; a backslash may be the window's escape of a
    // control character (`\n` for a newline), which a literal target can equal. Either way the
    // recorded form may name another target.
    recorded.filter(|t| !t.contains('\u{FFFD}') && !t.contains('\\'))
}

/// The `created_dirs` that hold (or are) what `link` with the target text `target` points to.
/// A relative target is joined to the link's directory. The target is resolved by the file
/// system, component by component, never by text: `..` after a linked component then moves
/// from where that link leads. A target that does not resolve vouches for nothing.
fn vouching<'a>(link: &Path, target: &str, created_dirs: &[&'a Path]) -> Vec<&'a Path> {
    let Some(to) = in_order(&link.parent().unwrap_or(link).join(target)) else {
        return Vec::new();
    };
    let to = to.to_string_lossy();
    created_dirs
        .iter()
        .copied()
        // Both sides are resolved on disk, so the comparison is exact: a case fold would let
        // `~/foo/file` count as inside `~/Foo` on a case-sensitive volume.
        .filter(|d| in_order(d).is_some_and(|d| Path::new(to.as_ref()).starts_with(&d)))
        .collect()
}

/// `p` resolved in the order the kernel walks it: each existing component is followed through
/// its links (`canonicalize`), and `..` then leaves where that link led. From the first missing
/// component on, the rest is joined by text, which is exact there because a missing name is not
/// a link. `None` when a component cannot be read, a link on the way does not resolve, or `..`
/// follows a missing component (the kernel does not resolve it either).
fn in_order(p: &Path) -> Option<PathBuf> {
    let mut cur = PathBuf::new();
    let mut missing = false;
    for c in p.components() {
        match c {
            Component::RootDir | Component::Prefix(_) => cur = PathBuf::from("/"),
            Component::CurDir => {}
            // The kernel fails `..` after a missing name (ENOENT), so text must not resolve it.
            Component::ParentDir if missing => return None,
            Component::ParentDir => {
                cur.pop();
            }
            Component::Normal(n) => {
                let next = cur.join(n);
                cur = if missing {
                    next
                } else {
                    match fs::symlink_metadata(&next) {
                        Ok(_) => fs::canonicalize(&next).ok()?,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            missing = true;
                            next
                        }
                        Err(_) => return None,
                    }
                };
            }
        }
    }
    Some(cur)
}

/// Each link item that only the created-directory rule authorizes, with the created directories
/// that vouch for it.
type Vouched = Vec<(String, Vec<String>)>;

/// Keeps each link item that only `vouched` authorizes (its created directories, by link path)
/// once none of those directories is still removed as a tree. Returns whether an item changed.
fn unvouch(items: &mut [PathItem], vouched: &Vouched) -> bool {
    let removed: Vec<String> = items
        .iter()
        .filter(|i| matches!((&i.now, &i.step), (Now::Dir(..), Step::Do)))
        .map(|i| i.path.clone())
        .collect();
    let mut changed = false;
    for i in items.iter_mut().filter(|i| i.step == Step::Do) {
        let Some((_, dirs)) = vouched.iter().find(|(l, _)| same_path(l, &i.path)) else {
            continue;
        };
        if dirs.iter().any(|d| removed.iter().any(|r| same_path(r, d))) {
            continue;
        }
        let dir = dirs.first().map_or_else(String::new, |d| visible(d));
        i.step = Step::Keep(format!("points into {dir}, which is kept"));
        changed = true;
    }
    changed
}

/// `p` with its parent resolved through links, so it compares with the canonical protected
/// paths. `p` itself is not followed.
fn real_parent(p: &Path) -> PathBuf {
    match (p.parent(), p.file_name()) {
        (Some(d), Some(n)) => fs::canonicalize(d).map_or_else(|_| p.to_path_buf(), |d| d.join(n)),
        _ => p.to_path_buf(),
    }
}

/// The trees a script trial's sandbox protected that `check_claim` does not cover: the guarded
/// `checkouts` and the directory of the dfm executable `exe`, each canonical so it compares with
/// [`real_parent`]. A path that cannot be resolved is kept as given.
fn protected_trees(checkouts: Vec<PathBuf>, exe: Option<PathBuf>) -> Vec<PathBuf> {
    let real = |p: PathBuf| fs::canonicalize(&p).unwrap_or(p);
    checkouts
        .into_iter()
        .map(real)
        .chain(
            exe.map(real)
                .and_then(|e| e.parent().map(Path::to_path_buf)),
        )
        .collect()
}

/// The kind name the observation window uses for `m`.
fn kind_of(m: &fs::Metadata) -> &'static str {
    let ft = m.file_type();
    if ft.is_symlink() {
        "link"
    } else if ft.is_dir() {
        "dir"
    } else if ft.is_file() {
        "file"
    } else {
        "other"
    }
}

/// What purge does with the entries of a script trial's window: each `created` entry below
/// HOME that the watch set could have seen appear is a path item (a candidate the digest
/// covers), every other entry is a `retained:` line. The window is evidence, not authority: an
/// entry `before` already holds, one no watched place could report, one in a place `before`
/// could not read, and one that overlaps a `protected` tree is never removed. Each `retained:`
/// line comes with its path, for `hold`. `skip` are the baseline paths, which the
/// record's own path items already cover; a window entry is checked against them only after
/// every retention rule, so a retained baseline path still yields its `retained:` line. A
/// `truncated` window may omit what a created directory holds, so no created directory of it
/// is removed; neither is one when `no_trees` gives another reason. A link repointed into a directory counts as the trial's only when that
/// directory is itself removed: the third list names, for each link only that rule
/// authorizes, the directories that vouch for it, for [`unvouch`]. A candidate is removed only
/// when it, and every entry of a directory's tree, passes the record's inode fence
/// ([`fence_keep`]): a path the script moved in from elsewhere keeps its older inode number.
/// `no_trees`, when set, says why no created directory is removed as a tree.
#[expect(
    clippy::too_many_arguments,
    reason = "the inputs of one decision; a struct would only rename them"
)]
fn script_items(
    s: &TrialScript,
    entries: &[WindowEntry],
    no_trees: Option<&str>,
    skip: &[&str],
    protected: &[PathBuf],
    owned: &[String],
    others: &[(String, String)],
    seen: &[Seen],
    store: &Store,
    sys: &Sys,
    home: &Path,
) -> (Vec<PathItem>, Vec<(String, String)>, Vouched) {
    let agents = home.join("Library/LaunchAgents");
    let watch = window::watch_set(home, &s.env);
    // Every path comparison here uses the case fold of the ownership checks: on a
    // case-insensitive volume `~/bin/Keep` and `~/bin/keep` are one file.
    let same = |a: &Path, b: &Path| same_path(&a.to_string_lossy(), &b.to_string_lossy());
    let watched = |p: &Path| {
        watch.iter().any(|w| match w {
            Watch::File(f) => same(f, p),
            Watch::Dir(d) | Watch::Listing(d) => p.parent().is_some_and(|q| same(q, d)),
        })
    };
    let before = |p: &str| s.before.iter().find(|b| same_path(&b.path, p));
    // The watched parent of `p` when `before` could not read it.
    let unread = |p: &Path| -> Option<String> {
        let d = p.parent()?.to_string_lossy().into_owned();
        before(&d)
            .is_some_and(|b| b.kind == "unreadable")
            .then_some(d)
    };
    let removed: Vec<&str> = entries
        .iter()
        .filter(|e| e.change == "removed")
        .map(|e| e.path.as_str())
        .collect();
    let (mut candidates, mut retained) = (Vec::new(), Vec::new());
    for e in entries {
        let (p, shown) = (Path::new(&e.path), visible(&e.path));
        match e.change.as_str() {
            "created" => {}
            "modified" => {
                // `shrink_window` may have dropped the entry's own `before`.
                let old = e.before.as_ref().or_else(|| before(&e.path));
                let now = window::observe(&[Watch::File(p.to_path_buf())])
                    .ok()
                    .and_then(|o| o.into_iter().next());
                let or_none = |h: Option<&String>| h.map_or_else(|| "none".into(), |h| visible(h));
                let mut what = format!(
                    "sha256 {} -> {}",
                    or_none(old.and_then(|o| o.sha256.as_ref())),
                    or_none(now.as_ref().and_then(|o| o.sha256.as_ref()))
                );
                let (t0, t1) = (
                    old.and_then(|o| o.target.as_ref()),
                    now.as_ref().and_then(|o| o.target.as_ref()),
                );
                if t0.is_some() || t1.is_some() {
                    what += &format!(", target {} -> {}", or_none(t0), or_none(t1));
                }
                retained.push((
                    &e.path,
                    format!("{shown} modified during the trial ({what}); edit {shown} by hand"),
                ));
                continue;
            }
            change => {
                retained.push((
                    &e.path,
                    format!("{shown} {change} during the trial; never restored"),
                ));
                continue;
            }
        }
        if e.kind == "unreadable" {
            retained.push((
                &e.path,
                format!("{shown} could not be read after the run; never removed"),
            ));
            continue;
        }
        if inexact(e) {
            retained.push((
                &e.path,
                format!("{shown} (the name was not recorded exactly)"),
            ));
            continue;
        }
        if !at_or_below(&e.path, &home.to_string_lossy()) || same(p, home) {
            retained.push((
                &e.path,
                format!("{shown} created outside HOME; never removed"),
            ));
            continue;
        }
        if before(&e.path).is_some() {
            retained.push((&e.path, format!(
                "{shown} is in the observation before the run, so the trial did not create it; never removed"
            )));
            continue;
        }
        if let Some(r) = renamed_from(p, &removed) {
            retained.push((&e.path, format!(
                "{shown} (the same file as {}, which existed before the trial; renamed during the trial)",
                visible(r)
            )));
            continue;
        }
        if let Some(d) = unread(p) {
            retained.push((&e.path, format!(
                "{shown} is in {}, which could not be read before the run, so the trial may not have created it; never removed",
                visible(&d)
            )));
            continue;
        }
        if !watched(p) {
            retained.push((&e.path, format!(
                "{shown} is not in a watched place, so the window cannot have seen it appear; never removed"
            )));
            continue;
        }
        if p.parent().is_some_and(|d| same(d, &agents)) {
            // The command is built from the exact path, single-quoted so a paste passes it as one
            // word; a path the terminal form changes (escapes) gets no command to copy.
            let line = if shown == e.path {
                let word = e.path.replace('\'', "'\\''");
                format!(
                    "{shown} created during the trial; unload it with launchctl bootout gui/{} '{word}', then remove it by hand",
                    sys.uid
                )
            } else {
                format!(
                    "{shown} created during the trial; its name cannot be shown exactly, so unload it with launchctl bootout by hand, then remove it"
                )
            };
            retained.push((&e.path, line));
            continue;
        }
        let real = real_parent(p).to_string_lossy().into_owned();
        if store.check_claim(&e.path).is_err()
            || protected
                .iter()
                .any(|t| overlaps(&t.to_string_lossy(), &real))
        {
            retained.push((
                &e.path,
                format!("{shown} is a path of the manager or a guarded checkout; never removed"),
            ));
            continue;
        }
        // A created entry that is now another kind is not what the window saw.
        if let Ok(m) = fs::symlink_metadata(p)
            && kind_of(&m) != e.kind
        {
            retained.push((
                &e.path,
                format!(
                    "{shown} created as a {} during the trial and now a {}; not removed",
                    e.kind,
                    kind_of(&m)
                ),
            ));
            continue;
        }
        if let Some(why) = fence_keep(p, s.fence) {
            retained.push((&e.path, format!("{shown} ({why})")));
            continue;
        }
        // A cut window may omit children purge must keep, and a directory goes as a tree.
        if let (Some(why), "dir") = (no_trees, e.kind.as_str()) {
            retained.push((&e.path, format!("{shown} ({why})")));
            continue;
        }
        // Only now, after every retention check: a retained baseline path must reach the
        // merged list so its baseline item is dropped.
        if skip.iter().any(|k| same_path(k, &e.path)) {
            continue;
        }
        candidates.push(e);
    }
    let item = |e: &WindowEntry, created_dirs: &[&Path]| {
        let keep = |t: &str| link_keep(Path::new(&e.path), e.target.as_deref(), t, created_dirs);
        let b = BaselinePath {
            path: e.path.clone(),
            existed: false,
            sha256: None,
        };
        PathItem {
            fence: s.fence,
            ..path_item(&b, owned, others, seen, sys, home, Some(&keep))
        }
    };
    // Only a created directory that is itself removed vouches for a link into it, so the links
    // are decided after every directory.
    let first: Vec<PathItem> = candidates.iter().map(|e| item(e, &[])).collect();
    let created_dirs: Vec<&Path> = candidates
        .iter()
        .zip(&first)
        .filter(|(_, i)| matches!((&i.now, &i.step), (Now::Dir(..), Step::Do)))
        .map(|(e, _)| Path::new(&e.path))
        .collect();
    let items: Vec<PathItem> = candidates
        .iter()
        .zip(first)
        .map(|(e, i)| match i.now {
            Now::Link(_) => item(e, &created_dirs),
            _ => i,
        })
        .collect();
    let vouched = candidates
        .iter()
        .zip(&items)
        .filter_map(|(e, i)| match (&i.now, &i.step) {
            (Now::Link(t), Step::Do) if lossless(e.target.as_deref()) != Some(t.as_str()) => {
                let dirs = vouching(Path::new(&e.path), t, &created_dirs);
                Some((
                    e.path.clone(),
                    dirs.iter()
                        .map(|d| d.to_string_lossy().into_owned())
                        .collect(),
                ))
            }
            _ => None,
        })
        .collect();
    let retained = retained.into_iter().map(|(p, l)| (p.clone(), l)).collect();
    (items, retained, vouched)
}

/// Why the window candidate `p` fails the inode `fence`, or `None` when it passes: `p` and, for
/// a directory, every entry of its tree (walked without following a link) are on the fence's
/// device with an inode number at or above the fence's, so the script created them. A missing
/// `p` passes: there is nothing to remove. The walk keeps the limits of [`tree`].
fn fence_keep(p: &Path, fence: Option<InodeFence>) -> Option<String> {
    let Some(f) = fence else {
        return Some("the record has no inode fence".into());
    };
    let unread =
        |e: std::io::Error| format!("it cannot be read for the inode check ({})", e.kind());
    let check = |q: &Path, m: &fs::Metadata| -> Result<(), String> {
        let why = if m.dev() != f.dev {
            "is not on the APFS volume the fence was taken on"
        } else if m.ino() < f.ino {
            "existed before the trial (inode older than the run); it may have been moved in"
        } else {
            return Ok(());
        };
        Err(if q == p {
            why.strip_prefix("is ")
                .unwrap_or(&format!("it {why}"))
                .to_string()
        } else {
            format!("it holds {}, which {why}", visible(&q.to_string_lossy()))
        })
    };
    let walk = || -> Result<(), String> {
        let m = match fs::symlink_metadata(p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            m => m.map_err(unread)?,
        };
        check(p, &m)?;
        if !m.is_dir() {
            return Ok(());
        }
        let (mut stack, mut count, mut listed) = (vec![p.to_path_buf()], 0usize, 0usize);
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).map_err(unread)? {
                let q = e.map_err(unread)?.path();
                let m = fs::symlink_metadata(&q).map_err(unread)?;
                check(&q, &m)?;
                (count, listed) = (count + 1, listed + q.as_os_str().len());
                if count > MAX_ENTRIES || listed > MAX_TREE_LIST_BYTES {
                    return Err("its tree is too large for the inode check".into());
                }
                if m.is_dir() {
                    stack.push(q);
                }
            }
        }
        Ok(())
    };
    walk().err()
}

/// Whether the window recorded `e` by a shown form that may name another file: the kind
/// `other` (the window's mark for a name that is not UTF-8 or holds a control character), or
/// U+FFFD in the path, which a lossy conversion leaves.
fn inexact(e: &WindowEntry) -> bool {
    e.kind == "other" || e.path.contains('\u{FFFD}')
}

/// The `removed` path that is now the same file as `created` (equal `(dev, ino)` from an
/// `lstat` of each). It catches a case-only or normalization-only rename on any volume. When
/// either `lstat` fails the guard does not apply.
fn renamed_from<'a>(created: &Path, removed: &[&'a str]) -> Option<&'a str> {
    let id = |p: &Path| fs::symlink_metadata(p).ok().map(|m| (m.dev(), m.ino()));
    let want = id(created)?;
    removed
        .iter()
        .copied()
        .find(|r| id(Path::new(r)) == Some(want))
}

/// A directory item is removed as a tree, so one that holds a kept item or a `retained` path
/// (strictly below it, case fold) stays: it moves out of `items` and is returned with its
/// `retained:` line. So does any item (a directory, a file or a link) that a kept or retained
/// link points into or at, with a relative target resolved by text against the link's
/// directory and again through the links on disk (`resolved`, and with the target's own last
/// component kept, `via_parents`), so an alias such as `~/.alias -> ~/.config` is seen:
/// removing it would leave that link dangling. A directory's other candidates are their own
/// items and are still removed one by one. The caller adds what is returned to `retained` and
/// calls again, since a held link can hold another item.
fn hold(items: &mut Vec<PathItem>, retained: &[(String, String)]) -> Vec<(String, String)> {
    let kept: Vec<String> = retained
        .iter()
        .map(|(p, _)| p.clone())
        .chain(
            items
                .iter()
                .filter(|i| matches!(i.step, Step::Keep(_)))
                .map(|i| i.path.clone()),
        )
        .collect();
    let s = |p: PathBuf| p.to_string_lossy().into_owned();
    let links: Vec<(&String, [String; 3])> = kept
        .iter()
        .filter_map(|k| {
            let t = fs::read_link(k).ok()?;
            let p = Path::new(k);
            let to = p.parent().unwrap_or(p).join(t);
            Some((k, [s(lexical(&to)), s(resolved(&to)), s(via_parents(&to))]))
        })
        .collect();
    let mut held = Vec::new();
    items.retain(|i| {
        if i.step != Step::Do {
            return true;
        }
        let dir = matches!(i.now, Now::Dir(..));
        // The item itself is not followed: a link item names the link, not its target.
        let real = s(via_parents(Path::new(&i.path)));
        let why = if let Some(k) = kept
            .iter()
            .find(|k| {
                dir && !same_path(k, &i.path)
                    && (at_or_below(k, &i.path)
                        // A kept path reached through a linked parent names the directory by
                        // another spelling.
                        || at_or_below(&s(via_parents(Path::new(k))), &s(resolved(Path::new(&i.path)))))
            })
        {
            format!("holds {}, which is kept", visible(k))
        } else if let Some((l, _)) = links.iter().find(|(_, [t, r, v])| {
            at_or_below(t, &i.path) || at_or_below(r, &real) || at_or_below(v, &real)
        }) {
            let how = if dir { "into" } else { "at" };
            format!("the kept link {} points {how} it", visible(l))
        } else {
            return true;
        };
        held.push((i.path.clone(), format!("{} ({why})", visible(&i.path))));
        false
    });
    held
}

/// `p` with every link on disk resolved: the longest existing ancestor is canonicalized and the
/// rest is joined after `lexical`. A path that nothing resolves stays as `lexical` gives it.
fn resolved(p: &Path) -> PathBuf {
    // The file system resolves `..` after a linked component correctly; text does not.
    if let Ok(c) = fs::canonicalize(p) {
        return c;
    }
    let p = lexical(p);
    let mut base = p.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(c) = fs::canonicalize(base) {
            return lexical(&rest.iter().rev().fold(c, |a, n| a.join(n)));
        }
        match (base.parent(), base.file_name()) {
            (Some(up), Some(n)) => {
                rest.push(n.to_os_string());
                base = up;
            }
            _ => return p,
        }
    }
}

/// The fingerprint of a regular file too large to hash: its size, plus its inode and ctime so a
/// same-length rewrite after the plan is stale (ctime changes on every write and cannot be set).
fn big_file(m: &fs::Metadata) -> String {
    format!(
        "size:{} ino:{} ctime:{}.{}",
        m.len(),
        m.ino(),
        m.ctime(),
        m.ctime_nsec()
    )
}

/// `p` with the links among its parents resolved and its own last component kept as it is, so a
/// kept link is not followed to its target.
fn via_parents(p: &Path) -> PathBuf {
    match (p.parent(), p.file_name()) {
        (Some(up), Some(n)) => resolved(up).join(n),
        _ => resolved(p),
    }
}

/// The fingerprint `path` has now, as the decision computes it, or `None` when it is not a
/// readable file or a directory whose tree can be listed.
fn live(path: &Path, uid: u32) -> Option<Now> {
    let m = fs::symlink_metadata(path).ok()?;
    if m.is_dir() {
        let (n, b, f) = tree(path, uid).ok()?.ok()?;
        Some(Now::Dir(n, b, f))
    } else if m.is_file() && m.len() > MAX_BYTES as u64 {
        Some(Now::File(big_file(&m)))
    } else if m.is_file() {
        fs::read(path).ok().map(|b| Now::File(file_fingerprint(&b)))
    } else {
        None
    }
}

/// Removes one path below HOME without following a link: every parent is opened as a real
/// directory owned by this user, then the entry is removed (a directory with its tree). The
/// entry must still be what the decision saw (`now`): a file or a tree with the same
/// fingerprint, a link with the same target text, and a window candidate still passes its
/// inode `fence` ([`fence_keep`]). Anything else is `E_PLAN_STALE` and nothing is removed.
fn remove_path(
    store: &Store,
    sys: &Sys,
    home: &Path,
    path: &str,
    now: &Now,
    fence: Option<InodeFence>,
    fail: &dfm_engine::Fail,
) -> Result<(), Refusal> {
    store.check_claim(path)?;
    let rel = Path::new(path)
        .strip_prefix(home)
        .ok()
        .and_then(|r| r.to_str())
        .ok_or_else(|| Refusal::new(Code::StateInvalid, format!("{path} is outside HOME")))?;
    let parts: Vec<&str> = rel.split('/').collect();
    let (leaf, parents) = parts
        .split_last()
        .ok_or_else(|| Refusal::new(Code::StateInvalid, format!("{path} is HOME itself")))?;
    let home_dir = Dir::open_home(sys, home)?;
    let parent = home_dir.open_below(sys, parents)?;
    fail.hit("purge-remove")?;
    // A window candidate must still pass the inode fence: nothing was moved in since.
    let fresh = || fence.is_none_or(|f| fence_keep(Path::new(path), Some(f)).is_none());
    let same = || fresh() && live(Path::new(path), sys.uid).as_ref() == Some(now);
    match (parent.peek(sys, leaf)?, now) {
        (Peek::Missing, _) => Ok(()),
        (Peek::Dir { .. }, Now::Dir(..)) if same() => parent.remove_tree(sys, leaf),
        (Peek::File { .. }, Now::File(_)) if same() => parent.remove_entry(leaf, fail),
        (Peek::Link { target, .. }, Now::Link(t)) if target == *t && fresh() => {
            parent.remove_entry(leaf, fail)
        }
        _ => Err(Refusal::new(
            Code::PlanStale,
            format!(
                "{path} changed after the decision; it is not removed. Fix: read the purge again with --preview"
            ),
        )),
    }
}

/// The trial record of `name`, or of `trial-<name>`. Missing, unreadable and adopted records
/// are refused.
fn trial_record(store: &Store, name: &str) -> Result<TrialRecord, Refusal> {
    let trials = store.load().trials;
    let find = |id: &str| {
        let file = format!("{id}.json");
        trials.iter().find(|(n, _)| *n == file)
    };
    let found = find(name).or_else(|| find(&format!("trial-{name}")));
    let t = match found {
        Some((_, Ok(t))) => t.clone(),
        Some((n, Err(e))) => {
            return Err(Refusal::new(
                Code::StateInvalid,
                format!("trials/{n} cannot be read: {}", e.detail),
            ));
        }
        None => {
            return Err(Refusal::new(
                Code::StateInvalid,
                format!(
                    "no trial {} in state/trials/; nothing was changed",
                    visible(name)
                ),
            ));
        }
    };
    if !t.is_active() {
        return Err(Refusal::new(
            Code::Conflict,
            format!(
                "trial {} is adopted; its files belong to a module now. Nothing was changed",
                t.trial_id
            ),
        ));
    }
    Ok(t)
}

pub fn run(ctx: &Ctx, args: &Args, out: &mut dyn Write) -> Result<(), Refusal> {
    let w =
        |e: std::io::Error| Refusal::new(Code::IoFailed, format!("output failed: {}", e.kind()));
    let binding = read_existing_binding(ctx)?.ok_or_else(|| {
        Refusal::new(
            Code::BindingInvalid,
            "this machine is not bound; run bind first",
        )
    })?;
    let binary = crate::plan_cmd::binary_identity()?;
    let stamp = Stamp::of(&binding);
    signal::install()?;
    let stop = || signal::requested().map_or(Ok(()), Err);
    let (layout, sys) = (ctx.layout(), ctx.sys()?);
    let runner = ctx.runner(&sys);
    let fail = state_cmd::fail_from_env()?;

    let decide = |store: &Store| -> Result<(TrialRecord, Decision), Refusal> {
        let t = trial_record(store, &args.trial)?;
        // A record written for another profile or machine is no authority to remove anything here.
        if !t.binding.matches(&binding) {
            return Err(Refusal::new(
                Code::IdentityMismatch,
                format!(
                    "trials/{}.json belongs to {} as {}, not {} as {}. Nothing was changed",
                    t.trial_id,
                    t.binding.profile,
                    t.binding.machine_id,
                    binding.profile(),
                    binding.machine_id()
                ),
            ));
        }
        // Another trial's record that cannot be read could name the same package or path, so
        // nothing is decided without it.
        let mut others = Vec::new();
        for (n, r) in store.load().trials {
            match r {
                Ok(o) if o.is_active() && o.trial_id != t.trial_id => others.push(o),
                Ok(_) => {}
                Err(e) => {
                    return Err(Refusal::new(
                        Code::StateInvalid,
                        format!(
                            "trials/{n} cannot be read ({}); it may share this trial's package or paths. Nothing was changed. Fix: repair or remove that record",
                            e.detail
                        ),
                    ));
                }
            }
        }
        let other_paths: Vec<(String, String)> = others
            .iter()
            .flat_map(|o| {
                let created = o
                    .script
                    .iter()
                    .flat_map(|s| s.window.iter().flatten())
                    .filter(|e| e.change == "created")
                    .map(|e| e.path.clone());
                // What that trial's links point at, as recorded and as they point now: purging
                // this trial must not leave another trial's link dangling.
                let targets: Vec<String> = o
                    .script
                    .iter()
                    .flat_map(|s| s.window.iter().flatten())
                    .flat_map(|e| {
                        let link = Path::new(&e.path);
                        let dir = link.parent().unwrap_or(link);
                        let live = fs::read_link(link).ok();
                        let recorded = e.target.as_deref().map(PathBuf::from);
                        recorded.into_iter().chain(live).flat_map(move |t| {
                            let to = dir.join(t);
                            let r = resolved(&to).to_string_lossy().into_owned();
                            [lexical(&to).to_string_lossy().into_owned(), r]
                        })
                    })
                    .collect();
                o.baseline
                    .paths
                    .iter()
                    .chain(&o.baseline.data)
                    .map(|b| b.path.clone())
                    .chain(created)
                    .chain(targets)
                    .map(|p| (o.trial_id.clone(), p))
            })
            .collect();
        // What another active script trial changed is never removed: the paths its window
        // names (`true`: they match in both directions), or, for a run that did not finish
        // (what it touched is unknown), every path in its `before`. A truncated window may
        // omit a change, so its `before` counts too.
        let other_seen: Vec<Seen> = others
            .iter()
            .flat_map(|o| {
                let truncated = o.unknown.iter().any(|u| u == window::WINDOW_TRUNCATED);
                o.script.iter().flat_map(move |s| {
                    let window = s.window.iter().flatten().map(|e| (&e.path, true));
                    let before = (s.window.is_none() || truncated)
                        .then(|| s.before.iter().map(|b| (&b.path, false)))
                        .into_iter()
                        .flatten();
                    let paths: Vec<(&String, bool)> = window.chain(before).collect();
                    paths
                        .into_iter()
                        .map(move |(p, w)| (o.trial_id.clone(), p.clone(), w))
                })
            })
            .collect();
        let used_by = |adapter: &str, name: &str| {
            others
                .iter()
                .find(|o| {
                    o.package.as_ref().is_some_and(|p| {
                        p.adapter == adapter
                            && (same_package(adapter, &p.name, name)
                                || p.install.as_ref().is_some_and(|i| {
                                    i.dependencies
                                        .iter()
                                        .any(|d| same_package(adapter, d, name))
                                }))
                    })
                })
                .map(|o| o.trial_id.clone())
        };
        let mut deps = Vec::new();
        let package = match &t.package {
            None => None,
            Some(p) => {
                let step = if t.baseline.tool_preexisting || p.install.is_none() {
                    Step::Keep(format!(
                        "existed before the trial ({})",
                        t.baseline.owners.join("; ")
                    ))
                } else if let Some(o) = used_by(&p.adapter, &p.name) {
                    Step::Keep(format!("trial {o} also uses it"))
                } else {
                    probe_step(&runner, &p.adapter, &p.name)?
                };
                if p.adapter == HOMEBREW
                    && matches!(step, Step::Do | Step::Absent)
                    && let Some(i) = &p.install
                {
                    for d in &i.dependencies {
                        stop()?;
                        let s = match used_by(HOMEBREW, d) {
                            Some(o) => Step::Keep(format!("trial {o} also uses it")),
                            None => probe_step(&runner, HOMEBREW, d)?,
                        };
                        deps.push((d.clone(), s));
                    }
                }
                Some((p.adapter.clone(), p.name.clone(), step))
            }
        };
        // Paths a module owns are never removed, even if the trial saw them absent.
        let owned: Vec<String> = store
            .read_ledger()?
            .resources
            .iter()
            .flat_map(|r| r.owned_outputs.iter().map(|o| o.path.clone()))
            .collect();
        let mut paths: Vec<PathItem> = t
            .baseline
            .paths
            .iter()
            .chain(&t.baseline.data)
            .map(|b| {
                path_item(
                    b,
                    &owned,
                    &other_paths,
                    &other_seen,
                    &sys,
                    &layout.home,
                    None,
                )
            })
            .collect();
        // A script could move a pre-existing file into a baseline path the window does not
        // watch (`~/Library/Caches/<tool>`), so a script trial's baseline candidates pass the
        // same inode fence as its window candidates, now and again just before removal.
        if let Some(s) = &t.script {
            for i in paths.iter_mut().filter(|i| i.step == Step::Do) {
                match fence_keep(Path::new(&i.path), s.fence) {
                    Some(why) => i.step = Step::Keep(why),
                    None => i.fence = s.fence,
                }
            }
        }
        // Another active trial with an inexact name in its window may hold a link this
        // purge cannot read back, pointing at any of this trial's candidates.
        let other_inexact = others.iter().find(|o| {
            o.script
                .iter()
                .flat_map(|s| s.window.iter().flatten())
                .any(inexact)
        });
        let other_why = other_inexact.map(|o| {
            format!(
                "script trial {} has a link name that cannot be read back; nothing the trial created is removed",
                o.trial_id
            )
        });
        let mut no_trees = t
            .unknown
            .iter()
            .any(|u| u == window::WINDOW_TRUNCATED)
            .then_some(TRUNCATED_WHY);
        let (mut retained, mut vouched) = (Vec::new(), Vec::new());
        if let Some(s) = &t.script {
            // A run that did not finish has no window: what was created since `before` is. The
            // live diff's link targets are today's, so no target counts as recorded.
            let live: Vec<WindowEntry>;
            // A run that ended but has no window failed its after-observation (a cap, for
            // example): what changed during the run is unknown, and a live diff now would also
            // count what appeared since. No window candidate is removed.
            let ended_unobserved = s.window.is_none() && s.exit.is_some();
            let entries = match &s.window {
                Some(w) => w,
                None if ended_unobserved => {
                    retained.push((
                        String::new(),
                        "window candidates (the run ended but its after-observation was not \
                         recorded; nothing it created is removed)"
                            .into(),
                    ));
                    live = Vec::new();
                    &live
                }
                None => {
                    let watch = window::watch_set(&layout.home, &s.env);
                    live = window::diff(&s.before, &window::observe(&watch)?)
                        .into_iter()
                        .map(|e| WindowEntry { target: None, ..e })
                        .collect();
                    &live
                }
            };
            // The trees the trial's sandbox protected that `check_claim` does not cover.
            let (root, _) = crate::canonical_root(&binding, None)?;
            let protected = protected_trees(
                crate::sync_cmd::guarded_checkouts(&root)?,
                std::env::current_exe().ok(),
            );
            let skip: Vec<&str> = paths.iter().map(|p| p.path.as_str()).collect();
            if entries.iter().any(inexact) {
                no_trees = no_trees.or(Some(INEXACT_WHY));
            }
            let (items, r, v) = script_items(
                s,
                entries,
                no_trees,
                &skip,
                &protected,
                &owned,
                &other_paths,
                &other_seen,
                store,
                &sys,
                &layout.home,
            );
            retained.extend(r);
            vouched = v;
            // No window candidate is removed when the window cannot be trusted as the run's
            // own effect: a window with no exit means the script never started (a failed
            // journal append or launch), so what appeared came from elsewhere; and a link with
            // an inexact name cannot be read back, so whatever it points at, file or directory,
            // could be left dangling.
            let truncated = t.unknown.iter().any(|u| u == window::WINDOW_TRUNCATED);
            let keep_all = if truncated {
                // A cut window may have dropped a link to any candidate, so none is removed.
                Some(TRUNCATED_WHY)
            } else if s.window.is_some() && s.exit.is_none() {
                Some(
                    "the script never started (no exit was recorded); nothing it created is removed",
                )
            } else if entries.iter().any(inexact) {
                Some(
                    "a window entry has a name that cannot be shown exactly; nothing the trial created is removed",
                )
            } else {
                None
            };
            paths.extend(items);
            // The trial's own baseline candidates too: an inexact link may point at one, and a
            // run that never started explains none of them.
            if let Some(why) = keep_all {
                paths.retain(|i| {
                    if i.step != Step::Do {
                        return true;
                    }
                    let line = format!("{} ({why})", visible(&i.path));
                    retained.push((i.path.clone(), line));
                    false
                });
            }
        }
        // Any trial's candidates, package trials included: that link may point at any of them.
        if let Some(why) = other_why.as_deref() {
            paths.retain(|i| {
                if i.step != Step::Do {
                    return true;
                }
                let line = format!("{} ({why})", visible(&i.path));
                retained.push((i.path.clone(), line));
                false
            });
        }
        // Another script trial whose run did not finish, or whose window was cut, may have
        // created any path again after its `before`, so no candidate is removed: neither a
        // window candidate nor a baseline path, whatever kind of trial this one is.
        let incomplete = others.iter().find(|o| {
            o.script.as_ref().is_some_and(|s| {
                s.window.is_none() || o.unknown.iter().any(|u| u == window::WINDOW_TRUNCATED)
            })
        });
        if let Some(o) = incomplete {
            paths.retain(|i| {
                if i.step != Step::Do {
                    return true;
                }
                let line = format!(
                    "{} (script trial {} is incomplete; what it changed is not recorded)",
                    visible(&i.path),
                    o.trial_id
                );
                retained.push((i.path.clone(), line));
                false
            });
        }
        // A retained path is never removed, whichever list named it: a baseline item for a
        // path a window rule retains (a LaunchAgents plist given as `--config`) goes.
        paths
            .retain(|i| i.step != Step::Do || !retained.iter().any(|(r, _)| same_path(r, &i.path)));
        // After the merge, so a kept baseline path also holds a created directory.
        // Holding a directory withdraws its vouch for a link into it, and keeping that link can
        // hold another item, as can a held link; so repeat until neither changes. Each round
        // that continues holds or keeps one more item, so the loop ends.
        loop {
            let h = hold(&mut paths, &retained);
            let more = !h.is_empty();
            retained.extend(h);
            if !unvouch(&mut paths, &vouched) && !more {
                break;
            }
        }
        let mut held = Vec::new();
        // A cut window may omit what any directory holds, a baseline one included, and a link
        // with an inexact name cannot be followed to the directory it holds; a directory goes
        // as a tree: none is removed. That withdraws each one's vouch for a link; no directory
        // is left for a kept link to hold, so one `unvouch` is enough.
        if let Some(why) = no_trees {
            paths.retain(|i| {
                let dir = matches!((&i.now, &i.step), (Now::Dir(..), Step::Do));
                if dir {
                    held.push(format!("{} ({why})", visible(&i.path)));
                }
                !dir
            });
            unvouch(&mut paths, &vouched);
        }
        let mut retained: Vec<String> = retained.into_iter().map(|(_, l)| l).collect();
        retained.extend(held);
        let d = Decision {
            trial_id: t.trial_id.clone(),
            tool: t.tool.clone(),
            package,
            deps,
            paths,
            retained,
            unknown: t.unknown.clone(),
            script: t.script.as_ref().map(|s| s.url.clone()),
            interrupted: t.script.as_ref().is_some_and(|s| s.window.is_none()),
            binary: binary.clone(),
            stamp: stamp.clone(),
        };
        Ok((t, d))
    };

    let readable =
        Store::open_read(sys, &layout, fail.clone())?.ok_or_else(crate::state_lost_refusal)?;
    readable.require_identity(&binding)?;
    let (rec, first) = decide(&readable)?;
    stop()?;
    let digest = first.digest()?;
    if let Some(i) = &ctx.isolated {
        writeln!(out, "isolated: true {i}").map_err(w)?;
    }
    first.print(out, &digest).map_err(w)?;
    if rec.baseline.data.is_empty() {
        writeln!(
            out,
            "note: this trial was recorded before data paths were observed; only its configuration paths are covered"
        )
        .map_err(w)?;
    }
    if let Some(approved) = &args.yes_plan
        && *approved != digest
    {
        return Err(stale(approved, &digest));
    }
    if args.preview {
        return Ok(());
    }
    let Some(approved) = &args.yes_plan else {
        return Err(Refusal::new(
            Code::ApprovalRequired,
            format!(
                "nothing was changed. Fix: read the purge above, then run again with --yes-plan {digest}"
            ),
        ));
    };
    drop(readable);

    let store = Store::open_write_with(sys, &layout, fail.clone(), false)?;
    if !store.is_initialized() {
        return Err(store.stamp(crate::state_lost_refusal()));
    }
    store
        .require_identity(&binding)
        .map_err(|r| store.stamp(r))?;
    let (_, locked) = decide(&store).map_err(|r| store.stamp(r))?;
    stop().map_err(|r| store.stamp(r))?;
    let locked_digest = locked.digest().map_err(|r| store.stamp(r))?;
    if locked_digest != digest {
        return Err(store.stamp(stale(approved, &locked_digest)));
    }
    let started = now_ns().map_err(|r| store.stamp(r))?;
    let run_id = format!("purge-{started}-{}", std::process::id());
    let trial_id = locked.trial_id.clone();
    let mut ops: Vec<OperationRecord> = Vec::new();
    let mut stopped: Option<Refusal> = None;

    // One action: its own operation id, an intent before it and an outcome after it, and one
    // receipt operation. `retain` turns brew's dependents refusal (`E_CONFLICT`) into a kept
    // item; any other failure fails the run. It answers the code of a failure, `None` on success.
    let mut act = |op: &str,
                   adapter: &str,
                   target: &str,
                   outputs: Vec<RecoveryOutput>,
                   retain: bool,
                   f: &dyn Fn() -> Result<(), Refusal>|
     -> Result<Option<Code>, Refusal> {
        let op_id = format!("{run_id}-{}", ops.len() + 1);
        let journal = |phase, recovery, reason| {
            store.append_journal(&JournalEntry {
                schema: SCHEMA,
                op_id: op_id.clone(),
                op: op.into(),
                phase,
                at: rfc3339(now_ns()?),
                resource: trial_id.clone(),
                adapter: adapter.into(),
                class: Class::Normal,
                recovery,
                token: None,
                reason,
            })
        };
        // A stop request starts no new action.
        if let Some(stop) = signal::requested() {
            return Err(stop);
        }
        let at = now_ns().unwrap_or(started);
        journal(
            Phase::Intent,
            Some(Recovery {
                outputs,
                snapshot: None,
            }),
            None,
        )?;
        let (status, error, failed) = match f() {
            Ok(()) => {
                journal(Phase::Done, None, None)?;
                (Status::Updated, None, None)
            }
            Err(e) => {
                let why = bounded_visible(&e.detail, 1024);
                journal(Phase::Failed, None, Some(why.clone()))?;
                if retain && e.code == Code::Conflict {
                    (Status::Current, Some(format!("kept: {why}")), Some(e.code))
                } else {
                    let code = e.code;
                    stopped.get_or_insert(e.after_mutation());
                    (Status::Failed, Some(why), Some(code))
                }
            }
        };
        // The receipt names the action even when the output line below cannot be written.
        let line = format!(
            "  {} {op} {} {op_id}{}",
            label(status),
            visible(target),
            error.as_ref().map_or(String::new(), |e| format!(": {e}"))
        );
        ops.push(OperationRecord {
            id: op_id,
            resource: trial_id.clone(),
            adapter: adapter.into(),
            class: Class::Normal,
            status,
            started_at: Some(rfc3339(at)),
            finished_at: Some(rfc3339(now_ns().unwrap_or(at))),
            before: None,
            after: None,
            owned_outputs: vec![],
            error,
            reload: None,
        });
        writeln!(out, "{line}").map_err(w)?;
        Ok(failed)
    };

    let journaled = (|| -> Result<(), Refusal> {
        if let Some((adapter, name, Step::Do)) = &locked.package {
            let failed = act("uninstall", adapter, name, vec![], false, &|| {
                package::uninstall(&runner, adapter, &package_value(name))?;
                match package::listed(&runner, adapter, &package_value(name))? {
                    PackageState::Missing(_) => Ok(()),
                    s => Err(Refusal::new(
                        Code::IoFailed,
                        format!(
                            "postcondition failed: {adapter} reports {} after the uninstall",
                            s.state()
                        ),
                    )),
                }
            })?;
            if failed.is_some() {
                // Nothing else runs: the dependencies and the files stay while the package
                // that uses them may still be there.
                return Ok(());
            }
        }
        // One brew call for every dependency: brew checks dependents against the whole set, so
        // a dependency of a dependency goes in the same call. When brew refuses, each one is
        // tried alone once and a refusal keeps it (an installed formula still needs it).
        let deps: Vec<String> = locked
            .deps
            .iter()
            .filter(|(_, s)| *s == Step::Do)
            .map(|(n, _)| n.clone())
            .collect();
        if !deps.is_empty() {
            let all = deps.join(" ");
            let failed = act("uninstall", HOMEBREW, &all, vec![], true, &|| {
                package::uninstall_formulae(&runner, &deps)
            })?;
            if failed == Some(Code::Conflict) {
                for n in &deps {
                    act("uninstall", HOMEBREW, n, vec![], true, &|| {
                        package::uninstall_formulae(&runner, std::slice::from_ref(n))
                    })?;
                }
            }
        }
        // A recorded path below a directory this run already removed went with that tree.
        // Parents go first, so a recorded path inside a tree this run removes goes with that tree
        // and never changes the tree's fingerprint before its own removal.
        let mut order: Vec<&PathItem> =
            locked.paths.iter().filter(|p| p.step == Step::Do).collect();
        order.sort_by_key(|p| Path::new(&p.path).components().count());
        let mut removed: Vec<&str> = Vec::new();
        for p in order {
            let below = |r: &&str| fold(&p.path).starts_with(&format!("{}/", fold(r)));
            if removed.iter().any(below) {
                continue;
            }
            // The target lets `state recover` settle an interrupted removal.
            let target = vec![RecoveryOutput {
                path: p.path.clone(),
                existed_before: true,
                expected_sha256: None,
            }];
            let failed = act("remove", "fs", &p.path, target, false, &|| {
                remove_path(&store, &sys, &layout.home, &p.path, &p.now, p.fence, &fail)
            })?;
            if failed.is_none() {
                removed.push(&p.path);
            }
        }
        Ok(())
    })();
    if let Err(e) = journaled {
        stopped.get_or_insert(store.stamp(e));
    }
    let mut ended = false;
    if stopped.is_none() {
        match store.remove_trial(&trial_id) {
            Ok(()) => ended = true,
            Err(e) => stopped = Some(e.after_mutation()),
        }
    }
    let finished = now_ns().unwrap_or(started);
    let exit = stopped.as_ref().map_or(Exit::Ok, Refusal::exit);
    let receipt_name = if ops.is_empty() && !ended {
        None
    } else {
        Some(store.write_receipt(&Receipt {
            schema: SCHEMA,
            op_id: run_id.clone(),
            command: "purge".into(),
            update: false,
            started_at: rfc3339(started),
            finished_at: rfc3339(finished),
            binary: BinaryId {
                version: binary.version.clone(),
                commit: option_env!("DFM_BUILD_COMMIT").unwrap_or("unknown").into(),
                sha256: binary.sha256.clone(),
            },
            profile: ProfileRef {
                id: stamp.profile.clone(),
                binding_digest: stamp.binding_digest.clone(),
            },
            machine_id: stamp.machine_id.clone(),
            plan_digest: Some(digest.clone()),
            approval: Some(Approval {
                kind: "yes_plan".into(),
                plan_digest: digest.clone(),
                at: rfc3339(started),
            }),
            sources: vec![],
            approvals: vec![],
            result: RunResult {
                outcome: exit.name().into(),
                exit_code: exit.code(),
                counts: count_statuses(&ops),
            },
            operations: ops,
        }))
    };
    if ended {
        writeln!(out, "trial: state/trials/{trial_id}.json removed").map_err(w)?;
    } else {
        writeln!(
            out,
            "trial: state/trials/{trial_id}.json kept; run dfm purge {trial_id} again after the fix"
        )
        .map_err(w)?;
    }
    let outcome = match receipt_name {
        None => stopped.map_or(Ok(()), Err),
        Some(Ok(name)) => {
            writeln!(out, "receipt: {}", visible(&name)).map_err(w)?;
            stopped.map_or(Ok(()), Err)
        }
        Some(Err(e)) => {
            let e = e.after_mutation();
            writeln!(out, "receipt: not written ({})", visible(&e.to_string())).map_err(w)?;
            match stopped {
                Some(r) if e.exit().combine(r.exit()) != e.exit() => Err(r),
                _ => Err(e),
            }
        }
    };
    let final_exit = outcome.as_ref().err().map_or(Exit::Ok, Refusal::exit);
    writeln!(
        out,
        "{} {}",
        paint::label(ctx.color, "result:"),
        paint::status(ctx.color, final_exit.name())
    )
    .map_err(w)?;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_trees_resolve_a_checkout_reached_through_a_link() {
        let base = std::env::temp_dir().join(format!("dfm-purge-protected-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("real/nested")).unwrap();
        std::os::unix::fs::symlink(base.join("real"), base.join("via")).unwrap();
        let missing = base.join("gone");
        let trees = protected_trees(
            vec![base.join("via/nested"), missing.clone()],
            Some(base.join("via/nested")),
        );
        let real = fs::canonicalize(base.join("real")).unwrap();
        assert_eq!(trees, [real.join("nested"), missing, real]);
        // A candidate below the checkout, seen through its real parent, overlaps it.
        let candidate = real_parent(&base.join("via/nested/x"));
        assert!(overlaps(
            &trees[0].to_string_lossy(),
            &candidate.to_string_lossy()
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn renamed_from_matches_a_removed_path_that_is_the_same_file() {
        let base = std::env::temp_dir().join(format!("dfm-purge-samefile-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        // Two hard links to one inode stand in for a case- or normalization-only rename.
        let (old, new, other) = (base.join("old"), base.join("new"), base.join("other"));
        fs::write(&old, "x").unwrap();
        fs::hard_link(&old, &new).unwrap();
        fs::write(&other, "y").unwrap();
        let (old_s, other_s, gone_s) = (
            old.to_str().unwrap(),
            other.to_str().unwrap(),
            base.join("gone").to_str().unwrap().to_owned(),
        );
        assert_eq!(renamed_from(&new, &[other_s, old_s]), Some(old_s));
        // A different file, a missing removed path and a missing created path do not match.
        assert_eq!(renamed_from(&new, &[other_s, &gone_s]), None);
        assert_eq!(renamed_from(&base.join("none"), &[old_s]), None);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn an_entry_of_kind_other_or_with_a_replacement_character_is_inexact() {
        let e = |path: &str, kind: &str| WindowEntry {
            path: path.into(),
            change: "created".into(),
            kind: kind.into(),
            sha256: None,
            target: None,
            before: None,
        };
        assert!(inexact(&e("/h/bin/x", "other")));
        assert!(inexact(&e("/h/bin/x\u{FFFD}y", "file")));
        assert!(!inexact(&e("/h/bin/x", "file")));
        assert!(!inexact(&e("/h/bin/caf\u{e9}", "dir")));
    }

    #[test]
    fn hold_dirs_holds_a_directory_a_kept_link_points_into() {
        let base = std::env::temp_dir().join(format!("dfm-purge-hold-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("bin")).unwrap();
        let (dir, other, link) = (base.join("cfg"), base.join("other"), base.join("bin/x"));
        // A relative target, resolved against the link's directory.
        std::os::unix::fs::symlink("../cfg/./v/x", &link).unwrap();
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let item = |p: &Path, now: Now, step: Step| PathItem {
            path: s(p),
            now,
            step,
            fence: None,
        };
        let tree = || Now::Dir(0, 0, String::new());
        let mut items = vec![
            item(&dir, tree(), Step::Do),
            item(&other, tree(), Step::Do),
            item(
                &link,
                Now::Link("../cfg/./v/x".into()),
                Step::Keep("repointed".into()),
            ),
        ];
        let held: Vec<String> = hold(&mut items, &[]).into_iter().map(|(_, l)| l).collect();
        assert_eq!(
            held,
            [format!(
                "{} (the kept link {} points into it)",
                s(&dir),
                s(&link)
            )]
        );
        let left: Vec<&str> = items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(left, [s(&other), s(&link)]);
        // A retained link counts too.
        let mut items = vec![item(&dir, tree(), Step::Do)];
        assert_eq!(hold(&mut items, &[(s(&link), String::new())]).len(), 1);
        assert!(items.is_empty());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn hold_dirs_holds_a_directory_a_kept_link_reaches_through_an_alias() {
        let base = std::env::temp_dir().join(format!("dfm-purge-alias-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("cfg/boat")).unwrap();
        fs::create_dir_all(base.join("bin")).unwrap();
        std::os::unix::fs::symlink(base.join("cfg"), base.join("alias")).unwrap();
        let (dir, link) = (base.join("cfg/boat"), base.join("bin/x"));
        std::os::unix::fs::symlink("../alias/boat/bin/x", &link).unwrap();
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let mut items = vec![
            PathItem {
                path: s(&dir),
                now: Now::Dir(0, 0, String::new()),
                step: Step::Do,
                fence: None,
            },
            PathItem {
                path: s(&link),
                now: Now::Link("../alias/boat/bin/x".into()),
                step: Step::Keep("repointed".into()),
                fence: None,
            },
        ];
        assert_eq!(hold(&mut items, &[]).len(), 1);
        assert_eq!(items.len(), 1);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_tree_whose_path_list_exceeds_the_budget_is_refused() {
        let base = std::env::temp_dir().join(format!("dfm-purge-cap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("a/b")).unwrap();
        fs::write(base.join("a/b/long-file-name"), "x").unwrap();
        let uid = fs::metadata(&base).unwrap().uid();
        assert!(tree_capped(&base, uid, 1 << 20).unwrap().is_ok());
        let refused = tree_capped(&base, uid, 8).unwrap().unwrap_err();
        assert!(refused.starts_with("its paths take more than"), "{refused}");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn hold_dirs_holds_a_directory_a_kept_file_reaches_through_a_linked_parent() {
        let base = std::env::temp_dir().join(format!("dfm-purge-kept-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("cfg/boat")).unwrap();
        fs::write(base.join("cfg/boat/rc"), "x").unwrap();
        std::os::unix::fs::symlink(base.join("cfg"), base.join("alias")).unwrap();
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let mut items = vec![
            PathItem {
                path: s(&base.join("cfg/boat")),
                now: Now::Dir(0, 0, String::new()),
                step: Step::Do,
                fence: None,
            },
            PathItem {
                path: s(&base.join("alias/boat/rc")),
                now: Now::File(String::new()),
                step: Step::Keep("a parent is unsafe".into()),
                fence: None,
            },
        ];
        assert_eq!(hold(&mut items, &[]).len(), 1);
        assert_eq!(items.len(), 1);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_link_target_through_an_alias_is_resolved_by_the_file_system() {
        let base = std::env::temp_dir().join(format!("dfm-purge-vouch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        // The created directory, and a look-alike outside it that the alias leads to.
        fs::create_dir_all(base.join("newdir")).unwrap();
        fs::write(base.join("newdir/file"), "x").unwrap();
        fs::create_dir_all(base.join("o/out/sub")).unwrap();
        fs::create_dir_all(base.join("o/newdir")).unwrap();
        fs::write(base.join("o/newdir/file"), "y").unwrap();
        fs::create_dir_all(base.join("bin")).unwrap();
        std::os::unix::fs::symlink(base.join("o/out"), base.join("alias")).unwrap();
        let link = base.join("bin/l");
        let created = base.join("newdir");
        let dirs = [created.as_path()];
        // By text this is `newdir/file`; the file system follows `alias` first.
        assert!(vouching(&link, "../alias/sub/../../newdir/file", &dirs).is_empty());
        assert_eq!(vouching(&link, "../newdir/file", &dirs), dirs);
        // A missing name is not a link, so a target not written yet still counts.
        assert_eq!(vouching(&link, "../newdir/gone", &dirs), dirs);
        // A dangling link on the way resolves to nothing, so it vouches for nothing.
        std::os::unix::fs::symlink(base.join("nowhere"), base.join("dangle")).unwrap();
        assert!(vouching(&link, "../dangle/../newdir/file", &dirs).is_empty());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_big_file_fingerprint_changes_on_a_same_length_rewrite() {
        let p = std::env::temp_dir().join(format!("dfm-purge-big-{}", std::process::id()));
        fs::write(&p, b"aaaa").unwrap();
        let before = big_file(&fs::symlink_metadata(&p).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&p, b"bbbb").unwrap();
        let after = big_file(&fs::symlink_metadata(&p).unwrap());
        assert!(before.starts_with("size:4 ") && after.starts_with("size:4 "));
        assert_ne!(before, after);
        fs::remove_file(&p).unwrap();
    }

    #[test]
    fn an_empty_directory_swapped_for_another_has_a_new_fingerprint() {
        let d = std::env::temp_dir().join(format!("dfm-purge-swap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir(&d).unwrap();
        let uid = fs::metadata(&d).unwrap().uid();
        let before = tree(&d, uid).unwrap().unwrap();
        assert_eq!(before.0, 0, "the root is not an entry");
        fs::remove_dir(&d).unwrap();
        fs::create_dir(&d).unwrap();
        let after = tree(&d, uid).unwrap().unwrap();
        assert_eq!(after.0, 0);
        assert_ne!(before.2, after.2);
        fs::remove_dir(&d).unwrap();
    }

    #[test]
    fn lexical_resolves_dot_and_dot_dot_by_text() {
        assert_eq!(
            lexical(Path::new("/h/.local/bin/../share/./c/v")),
            Path::new("/h/.local/share/c/v")
        );
    }

    #[test]
    fn link_keep_allows_the_recorded_target_and_targets_inside_a_created_dir() {
        let base = std::env::temp_dir().join(format!("dfm-purge-keep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        // Real directories, so the targets resolve on the file system as purge resolves them.
        for d in [".local/bin", ".local/share/c", ".local/share/new"] {
            fs::create_dir_all(base.join(d)).unwrap();
        }
        let h = fs::canonicalize(&base).unwrap();
        let hs = h.to_string_lossy().into_owned();
        let link = h.join(".local/bin/c");
        let (c, new) = (h.join(".local/share/c"), h.join(".local/share/new"));
        let dirs = [c.as_path(), new.as_path()];
        // The recorded target, even outside every created directory.
        assert_eq!(
            link_keep(&link, Some("/usr/bin/true"), "/usr/bin/true", &dirs),
            None
        );
        // A relative target into the created directory, and the directory itself.
        assert_eq!(link_keep(&link, Some("/x"), "../share/c/v2/c", &dirs), None);
        assert_eq!(
            link_keep(&link, None, &format!("{hs}/.local/share/c"), &dirs),
            None
        );
        // The created directory in another case, as a case-insensitive volume resolves it.
        assert_eq!(
            link_keep(&link, None, &format!("{hs}/.local/Share/C/v2"), &dirs),
            None
        );
        // `..` that leaves the created directory.
        assert_eq!(
            link_keep(&link, Some("/x"), "../share/c/../other", &dirs).as_deref(),
            Some("repointed since the trial")
        );
        // `..` after a missing name does not resolve, so by text alone it is not inside `new`.
        assert_eq!(
            link_keep(&link, Some("/x"), "../share/new/missing/../bin/tool", &dirs).as_deref(),
            Some("repointed since the trial")
        );
        // A name that only starts like the directory is not inside it.
        assert_eq!(
            link_keep(&link, None, &format!("{hs}/.local/share/cc/v"), &dirs).as_deref(),
            Some("no target recorded")
        );
        // An absolute target elsewhere.
        assert_eq!(
            link_keep(&link, Some("/x"), "/usr/bin/true", &dirs).as_deref(),
            Some("repointed since the trial")
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_lossy_recorded_target_is_no_removal_authority() {
        let link = Path::new("/h/.local/bin/c");
        // The window wrote U+FFFD for a target byte that is not UTF-8; a live target that holds
        // the character itself is another file.
        let lossy = "/opt/c\u{FFFD}";
        assert_eq!(
            link_keep(link, Some(lossy), lossy, &[]).as_deref(),
            Some("no target recorded")
        );
        // A created directory still vouches through the live, lossless target.
        let dirs = [Path::new("/opt")];
        assert_eq!(link_keep(link, Some(lossy), lossy, &dirs), None);
        assert_eq!(lossless(Some("/opt/c")), Some("/opt/c"));
    }

    #[test]
    fn fence_keep_passes_only_what_is_newer_than_the_fence() {
        let base = std::env::temp_dir().join(format!("dfm-purge-fence-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("old")).unwrap();
        fs::write(base.join("old/f"), "x").unwrap();
        let probe = base.join("probe");
        fs::write(&probe, "").unwrap();
        let m = fs::symlink_metadata(&probe).unwrap();
        let f = Some(InodeFence {
            dev: m.dev(),
            ino: m.ino(),
        });
        fs::create_dir_all(base.join("new/sub")).unwrap();
        fs::write(base.join("new/sub/f"), "y").unwrap();
        assert_eq!(fence_keep(&base.join("new"), f), None);
        assert_eq!(fence_keep(&base.join("gone"), f), None);
        assert_eq!(
            fence_keep(&base.join("old/f"), f).as_deref(),
            Some(
                "it existed before the trial (inode older than the run); it may have been moved in"
            )
        );
        // An old directory moved into a new one holds the new one.
        fs::rename(base.join("old"), base.join("new/sub/old")).unwrap();
        let held = fence_keep(&base.join("new"), f).unwrap();
        assert!(
            held.starts_with("it holds ") && held.contains("/new/sub/old, which existed"),
            "{held}"
        );
        assert_eq!(
            fence_keep(&base.join("new/sub/f"), None).as_deref(),
            Some("the record has no inode fence")
        );
        let other = f.map(|f| InodeFence {
            dev: f.dev + 1,
            ..f
        });
        assert_eq!(
            fence_keep(&base.join("new/sub/f"), other).as_deref(),
            Some("not on the APFS volume the fence was taken on")
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn hold_keeps_a_file_or_link_a_kept_link_points_at() {
        let base = std::env::temp_dir().join(format!("dfm-purge-at-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("bin")).unwrap();
        fs::create_dir_all(base.join("real")).unwrap();
        // `via` is a linked parent: `via/tool` is `real/tool`.
        std::os::unix::fs::symlink(base.join("real"), base.join("via")).unwrap();
        let (tool, other, inner) = (
            base.join("real/tool"),
            base.join("bin/other"),
            base.join("bin/inner"),
        );
        fs::write(&tool, "x").unwrap();
        fs::write(&other, "y").unwrap();
        std::os::unix::fs::symlink("/usr/bin/true", &inner).unwrap();
        let (alias, alias2) = (base.join("bin/alias"), base.join("bin/alias2"));
        std::os::unix::fs::symlink("../via/tool", &alias).unwrap();
        // A kept link at a link item holds the link, not what that link points to.
        std::os::unix::fs::symlink("inner", &alias2).unwrap();
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let item = |p: &Path, now: Now, step: Step| PathItem {
            path: s(p),
            now,
            step,
            fence: None,
        };
        let mut items = vec![
            item(&tool, Now::File(String::new()), Step::Do),
            item(&other, Now::File(String::new()), Step::Do),
            item(&inner, Now::Link("/usr/bin/true".into()), Step::Do),
            item(
                &alias,
                Now::Link("../via/tool".into()),
                Step::Keep("repointed".into()),
            ),
        ];
        let held: Vec<String> = hold(&mut items, &[(s(&alias2), String::new())])
            .into_iter()
            .map(|(_, l)| l)
            .collect();
        assert_eq!(
            held,
            [
                format!("{} (the kept link {} points at it)", s(&tool), s(&alias)),
                format!("{} (the kept link {} points at it)", s(&inner), s(&alias2)),
            ]
        );
        let left: Vec<&str> = items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(left, [s(&other), s(&alias)]);
        fs::remove_dir_all(&base).unwrap();
    }
}
