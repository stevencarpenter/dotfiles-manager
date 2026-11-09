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
    Approval, BaselinePath, BinaryId, Class, JournalEntry, MAX_BYTES, OperationRecord, Phase,
    ProfileRef, Receipt, Recovery, RecoveryOutput, RunResult, SCHEMA, Stamp, Status, TrialRecord,
    TrialScript, WindowEntry, count_statuses, rfc3339,
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
                Now::File(h) if h.starts_with("size:") => format!(" file ({h})"),
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

/// The tree below `dir`: entries, bytes and a fingerprint of every relative path, kind and size,
/// without following a link. `Ok(Err(reason))` when the tree cannot be removed whole: more than
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
    let mut lines = Vec::new();
    let mut listed = 0usize;
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
            let line = format!("{rel}\t{kind}\t{}", m.len());
            listed += line.len();
            lines.push(line);
            if lines.len() > MAX_ENTRIES {
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
        lines.len(),
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
        Now::File(format!("size:{}", m.len()))
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
/// may unlink it: the target is the `recorded` one, or lies inside (or is) one of
/// `created_dirs`. A relative target is joined to the link's directory and resolved by text.
fn link_keep(
    link: &Path,
    recorded: Option<&str>,
    target: &str,
    created_dirs: &[&Path],
) -> Option<String> {
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

/// The `created_dirs` that hold (or are) what `link` with the target text `target` points to.
/// A relative target is joined to the link's directory and resolved by text.
fn vouching<'a>(link: &Path, target: &str, created_dirs: &[&'a Path]) -> Vec<&'a Path> {
    let to = lexical(&link.parent().unwrap_or(link).join(target));
    let to = to.to_string_lossy();
    created_dirs
        .iter()
        .copied()
        .filter(|d| at_or_below(&to, &d.to_string_lossy()))
        .collect()
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
/// line comes with its path, for `hold_dirs`. `skip` are the baseline paths, which the
/// record's own path items already cover; a window entry is checked against them only after
/// every retention rule, so a retained baseline path still yields its `retained:` line. A
/// `truncated` window may omit what a created directory holds, so no created directory of it
/// is removed. A link repointed into a directory counts as the trial's only when that
/// directory is itself removed: the third list names, for each link only that rule
/// authorizes, the directories that vouch for it, for [`unvouch`].
#[expect(
    clippy::too_many_arguments,
    reason = "the inputs of one decision; a struct would only rename them"
)]
fn script_items(
    s: &TrialScript,
    entries: &[WindowEntry],
    truncated: bool,
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
            // Single-quoted, so a pasted command passes the path as one word.
            let word = shown.replace('\'', "'\\''");
            retained.push((&e.path, format!(
                "{shown} created during the trial; unload it with launchctl bootout gui/{} '{word}', then remove it by hand",
                sys.uid
            )));
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
        // A cut window may omit children purge must keep, and a directory goes as a tree.
        if truncated && e.kind == "dir" {
            retained.push((
                &e.path,
                format!("{shown} (the window was truncated; its contents were not all recorded)"),
            ));
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
        path_item(&b, owned, others, seen, sys, home, Some(&keep))
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
            (Now::Link(t), Step::Do) if e.target.as_deref() != Some(t.as_str()) => {
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
/// (strictly below it, case fold) stays: it moves out of `items` and becomes a `retained:`
/// line. So does one that a kept or retained link points into (or at), with a relative
/// target resolved by text against the link's directory and again through the links on disk
/// (`resolved`), so an alias such as `~/.alias -> ~/.config` is seen: removing it would leave
/// that link dangling. Its other candidates are their own items and are still removed one by
/// one.
fn hold_dirs(items: &mut Vec<PathItem>, retained: &[(String, String)]) -> Vec<String> {
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
    let links: Vec<(&String, String, String)> = kept
        .iter()
        .filter_map(|k| {
            let t = fs::read_link(k).ok()?;
            let p = Path::new(k);
            let to = p.parent().unwrap_or(p).join(t);
            Some((k, s(lexical(&to)), s(resolved(&to))))
        })
        .collect();
    let mut held = Vec::new();
    items.retain(|i| {
        if !matches!((&i.now, &i.step), (Now::Dir(..), Step::Do)) {
            return true;
        }
        let why = if let Some(k) = kept
            .iter()
            .find(|k| !same_path(k, &i.path) && at_or_below(k, &i.path))
        {
            format!("holds {}, which is kept", visible(k))
        } else if let Some((l, ..)) = links.iter().find(|(_, t, r)| {
            at_or_below(t, &i.path) || at_or_below(r, &s(resolved(Path::new(&i.path))))
        }) {
            format!("the kept link {} points into it", visible(l))
        } else {
            return true;
        };
        held.push(format!("{} ({why})", visible(&i.path)));
        false
    });
    held
}

/// `p` with every link on disk resolved: the longest existing ancestor is canonicalized and the
/// rest is joined after `lexical`. A path that nothing resolves stays as `lexical` gives it.
fn resolved(p: &Path) -> PathBuf {
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

/// The fingerprint `path` has now, as the decision computes it, or `None` when it is not a
/// readable file or a directory whose tree can be listed.
fn live(path: &Path, uid: u32) -> Option<Now> {
    let m = fs::symlink_metadata(path).ok()?;
    if m.is_dir() {
        let (n, b, f) = tree(path, uid).ok()?.ok()?;
        Some(Now::Dir(n, b, f))
    } else if m.is_file() && m.len() > MAX_BYTES as u64 {
        Some(Now::File(format!("size:{}", m.len())))
    } else if m.is_file() {
        fs::read(path).ok().map(|b| Now::File(file_fingerprint(&b)))
    } else {
        None
    }
}

/// Removes one path below HOME without following a link: every parent is opened as a real
/// directory owned by this user, then the entry is removed (a directory with its tree). The
/// entry must still be what the decision saw (`now`): a file or a tree with the same
/// fingerprint, a link with the same target text. Anything else is `E_PLAN_STALE` and nothing
/// is removed.
fn remove_path(
    store: &Store,
    sys: &Sys,
    home: &Path,
    path: &str,
    now: &Now,
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
    let same = || live(Path::new(path), sys.uid).as_ref() == Some(now);
    match (parent.peek(sys, leaf)?, now) {
        (Peek::Missing, _) => Ok(()),
        (Peek::Dir { .. }, Now::Dir(..)) if same() => parent.remove_tree(sys, leaf),
        (Peek::File { .. }, Now::File(_)) if same() => parent.remove_entry(leaf, fail),
        (Peek::Link { target, .. }, Now::Link(t)) if target == *t => {
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
                o.baseline
                    .paths
                    .iter()
                    .chain(&o.baseline.data)
                    .map(|b| b.path.clone())
                    .chain(created)
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
        let truncated = t.unknown.iter().any(|u| u == window::WINDOW_TRUNCATED);
        let (mut retained, mut vouched) = (Vec::new(), Vec::new());
        if let Some(s) = &t.script {
            // A run that did not finish has no window: what was created since `before` is. The
            // live diff's link targets are today's, so no target counts as recorded.
            let live: Vec<WindowEntry>;
            let entries = match &s.window {
                Some(w) => w,
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
            let (items, r, v) = script_items(
                s,
                entries,
                truncated,
                &skip,
                &protected,
                &owned,
                &other_paths,
                &other_seen,
                store,
                &sys,
                &layout.home,
            );
            retained = r;
            vouched = v;
            paths.extend(items);
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
        // hold another directory, so repeat until neither changes. Each round that continues
        // keeps one more item, so the loop ends.
        let mut held = Vec::new();
        loop {
            held.extend(hold_dirs(&mut paths, &retained));
            if !unvouch(&mut paths, &vouched) {
                break;
            }
        }
        // A cut window may omit what any directory holds, a baseline one included, and a
        // directory goes as a tree: none is removed. That withdraws each one's vouch for a
        // link; no directory is left for a kept link to hold, so one `unvouch` is enough.
        if truncated {
            paths.retain(|i| {
                let dir = matches!((&i.now, &i.step), (Now::Dir(..), Step::Do));
                if dir {
                    held.push(format!(
                        "{} (the window was truncated; its contents were not all recorded)",
                        visible(&i.path)
                    ));
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
        let mut removed: Vec<&str> = Vec::new();
        for p in locked.paths.iter().filter(|p| p.step == Step::Do) {
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
                remove_path(&store, &sys, &layout.home, &p.path, &p.now, &fail)
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
        let held = hold_dirs(&mut items, &[]);
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
        assert_eq!(hold_dirs(&mut items, &[(s(&link), String::new())]).len(), 1);
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
            },
            PathItem {
                path: s(&link),
                now: Now::Link("../alias/boat/bin/x".into()),
                step: Step::Keep("repointed".into()),
            },
        ];
        assert_eq!(hold_dirs(&mut items, &[]).len(), 1);
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
    fn lexical_resolves_dot_and_dot_dot_by_text() {
        assert_eq!(
            lexical(Path::new("/h/.local/bin/../share/./c/v")),
            Path::new("/h/.local/share/c/v")
        );
    }

    #[test]
    fn link_keep_allows_the_recorded_target_and_targets_inside_a_created_dir() {
        let link = Path::new("/h/.local/bin/c");
        let dirs = [Path::new("/h/.local/share/c")];
        // The recorded target, even outside every created directory.
        assert_eq!(
            link_keep(link, Some("/usr/bin/true"), "/usr/bin/true", &dirs),
            None
        );
        // A relative target into the created directory, and the directory itself.
        assert_eq!(link_keep(link, Some("/x"), "../share/c/v2/c", &dirs), None);
        assert_eq!(link_keep(link, None, "/h/.local/share/c", &dirs), None);
        // The created directory in another case, as a case-insensitive volume resolves it.
        assert_eq!(link_keep(link, None, "/h/.local/Share/C/v2", &dirs), None);
        // `..` that leaves the created directory.
        assert_eq!(
            link_keep(link, Some("/x"), "../share/c/../other", &dirs).as_deref(),
            Some("repointed since the trial")
        );
        // A name that only starts like the directory is not inside it.
        assert_eq!(
            link_keep(link, None, "/h/.local/share/cc/v", &dirs).as_deref(),
            Some("no target recorded")
        );
        // An absolute target elsewhere.
        assert_eq!(
            link_keep(link, Some("/x"), "/usr/bin/true", &dirs).as_deref(),
            Some("repointed since the trial")
        );
    }
}
