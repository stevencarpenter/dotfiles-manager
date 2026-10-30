//! `adopt`: promotes a trial into a module (docs/adopt.md). Each regular file the trial created
//! or changed under its baseline paths is copied into the module's `home/` directory, gets an
//! `fs-link/v1` block appended to the module manifest, and its live file is replaced by a link
//! through the `apply` publish path. A `homebrew/v1` trial also gets a line appended to the
//! Brewfile. The trial then ends. Nothing is committed: the owner reviews the repo diff.
//!
//! The scan runs `gitleaks dir` over a manager-owned copy of the candidates, never over the
//! candidate directory. gitleaks reads `.gitleaks.toml` from the scanned directory, and a planted
//! one could allowlist everything; the copy holds only files named `0`, `1`, ... so no such file
//! can exist in it. An empty `--config` file is no fix: it replaces the default rules with none,
//! so the scan passes no `--config` and the runner clears `GITLEAKS_CONFIG`. The ignore file is
//! read from a named empty directory instead of the working directory.

use crate::adapters::package::{self, HOMEBREW};
use crate::apply_cmd::{is_digest, label, stale};
use crate::plan_cmd::{Probed, binary_identity, plan_with_outputs};
use crate::read_fs::read_regular_bounded;
use crate::sync_cmd::Git;
use crate::{Ctx, Loaded, load_resolved, paint, require_binding, state_cmd, usage};
use dfm_engine::{
    Approval as Allowance, Dir, End, ProcRequest, Run, Runner, Store, now_ns, signal,
};
use dfm_model::adopt::{BrewKind, append, brewfile_has, brewfile_line, link_block, resource_id};
use dfm_model::digest::sha256_hex;
use dfm_model::display::{bounded_visible, visible};
use dfm_model::plan::{
    Action, ApprovedFile, Binary, Plan, Scope, Status as PlanStatus, file_fingerprint,
};
use dfm_model::resolve::{Entry, module_manifest, resolve};
use dfm_model::state::{
    Approval, BinaryId, Class, DisruptiveApproval, Fingerprint, JournalEntry, MAX_BYTES,
    OperationRecord, Phase, ProfileRef, Receipt, Recovery, RunResult, SCHEMA, Stamp, Status,
    TrialAdopted, TrialRecord, count_statuses, rfc3339, valid_id,
};
use dfm_model::{Code, Exit, Refusal, fold};
use dfm_platform_macos::{O_NOFOLLOW, O_NONBLOCK};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const SCAN_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_REPORT: usize = 16 << 20;
/// gitleaks' own "leaks found" status, set with `--exit-code`: no other end can mean a finding.
const LEAKS: i32 = 3;

fn failed(detail: impl Into<String>) -> Refusal {
    Refusal::new(Code::ProbeFailed, detail)
}

fn io_failed(what: &str, e: &std::io::Error) -> Refusal {
    Refusal::new(Code::IoFailed, format!("{what}: {}", e.kind()))
}

fn text(p: &Path) -> Result<&str, Refusal> {
    p.to_str().ok_or_else(|| {
        crate::usage(format!(
            "{} is not a UTF-8 path; adopt passes paths to its tools as text",
            p.display()
        ))
    })
}

/// Removes the scan directory on every exit, an error and a stop included. A failure to remove
/// is dropped: the scan result decides the run. A SIGKILL leaves the directory behind; it stays
/// 0700 with 0600 files under the manager's cache directory.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Scans `files` (each an original path and its bytes) with the default gitleaks rules and
/// returns the original paths of the files with a finding, sorted. Neither the secret nor the
/// report reaches the result: `--redact` is set and only the file names are read.
/// `scan_dir` is the parent directory (`<cache dir>/adopt-scan`); the scan creates a fresh
/// `<pid>-<ns>` directory inside it. A missing `gitleaks`, a failed scan and a report that
/// cannot be mapped to the candidates are `E_PROBE_FAILED`.
pub(crate) fn scan(
    runner: &Runner,
    scan_dir: &Path,
    files: &[(String, Vec<u8>)],
) -> Result<Vec<String>, Refusal> {
    let private = |p: &Path, recursive: bool| {
        DirBuilder::new()
            .recursive(recursive)
            .mode(0o700)
            .create(p)
            .map_err(|e| io_failed(&format!("create {}", p.display()), &e))
    };
    private(scan_dir, true)?;
    let parent = fs::symlink_metadata(scan_dir)
        .map_err(|e| io_failed(&format!("read {}", scan_dir.display()), &e))?;
    if !parent.is_dir() || parent.uid() != runner.uid {
        return Err(failed(format!(
            "{} is not a directory of yours",
            scan_dir.display()
        )));
    }
    if parent.mode() & 0o7777 != 0o700 {
        fs::set_permissions(scan_dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| io_failed(&format!("protect {}", scan_dir.display()), &e))?;
    }
    let run = scan_dir.join(format!("{}-{}", std::process::id(), now_ns()?));
    private(&run, false)?;
    let _cleanup = Cleanup(run.clone());
    let (copy, ignore) = (run.join("files"), run.join("ignore"));
    private(&copy, false)?;
    private(&ignore, false)?;
    // Two copies of each candidate in `files/<i>/`. The numbered copy carries no file name, so
    // the default path allowlist (`*.svg`, lock files, a file named `.gitleaks.toml`) cannot
    // skip it. The copy with the original base name keeps the rules that match on a name or
    // extension (`cert.p12`). A finding in either maps back by the number.
    for (i, (original, bytes)) in files.iter().enumerate() {
        let dir = copy.join(i.to_string());
        private(&dir, false)?;
        let base = Path::new(original).file_name().unwrap_or("file".as_ref());
        let mut names = vec![PathBuf::from(i.to_string())];
        if base != i.to_string().as_str() {
            names.push(PathBuf::from(base));
        }
        for name in names {
            let path = dir.join(name);
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .and_then(|mut f| f.write_all(bytes))
                .map_err(|e| io_failed(&format!("copy a candidate to {}", path.display()), &e))?;
        }
    }
    let args: Vec<String> = [
        "dir",
        text(&copy)?,
        "--no-banner",
        "--redact",
        "--ignore-gitleaks-allow",
        "--exit-code",
        "3",
        "--gitleaks-ignore-path",
        text(&ignore)?,
        "--report-format",
        "json",
        "--report-path",
        "-",
    ]
    .map(String::from)
    .into();
    let out = runner.run(&ProcRequest {
        program: "gitleaks",
        args: &args,
        env: &[],
        stdin: None,
        timeout: SCAN_TIMEOUT,
        max_stdout: MAX_REPORT,
    })?;
    match out.end {
        End::Exited(0) => Ok(Vec::new()),
        End::Exited(LEAKS) => {
            let names: Vec<String> = files.iter().map(|(p, _)| p.clone()).collect();
            findings(&out.stdout, &copy, &names)
        }
        End::Exited(n) => Err(failed(format!("gitleaks exit {n}"))),
        End::Signaled => Err(failed("gitleaks was killed")),
        End::TimedOut => Err(failed("gitleaks timed out")),
        End::Stopped(r) => Err(r),
    }
}

/// The candidate paths named by a gitleaks JSON report of the copy in `dir`. A report that is
/// not a non-empty list of findings, or names a file that is not one of the copies, is refused:
/// exit 3 means a finding, so the scan must not read as clean.
fn findings(report: &[u8], dir: &Path, names: &[String]) -> Result<Vec<String>, Refusal> {
    let unreadable = || failed("gitleaks reported a finding that adopt cannot map to a candidate");
    let value: serde_json::Value = serde_json::from_slice(report).map_err(|_| unreadable())?;
    let list = value
        .as_array()
        .filter(|l| !l.is_empty())
        .ok_or_else(unreadable)?;
    let mut hit = BTreeSet::new();
    for item in list {
        let file = item
            .get("File")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(unreadable)?;
        let name = Path::new(file)
            .strip_prefix(dir)
            .ok()
            .and_then(|p| p.components().next())
            .and_then(|c| c.as_os_str().to_str())
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|i| names.get(i))
            .ok_or_else(unreadable)?;
        hit.insert(name.clone());
    }
    Ok(hit.into_iter().collect())
}

/// The paths of `rels` (relative to `root`) that have uncommitted changes: modified, staged,
/// deleted or untracked. `root` that is no repository is `E_PROBE_FAILED`.
pub(crate) fn dirty(git: &Git, root: &Path, rels: &[&str]) -> Result<Vec<String>, Refusal> {
    if rels.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec!["-C", text(root)?, "status", "--porcelain=v1", "-z", "--"];
    args.extend(rels);
    let out = git.run(true, Code::ProbeFailed, "git status", &args)?;
    let mut entries = out.split('\0');
    let mut paths = Vec::new();
    while let Some(entry) = entries.next() {
        let (Some(status), Some(path)) = (entry.get(..2), entry.get(3..)) else {
            continue;
        };
        paths.push(path.to_string());
        // A rename or copy entry is followed by its source path as a separate field.
        if status.bytes().any(|b| matches!(b, b'R' | b'C')) {
            entries.next();
        }
    }
    // git lists tracked changes before untracked paths; a sorted list is stable for the report.
    paths.sort();
    Ok(paths)
}

/// Whether `rel` under `root` belongs to the same git checkout as `root`, tracked or not: a path
/// that leaves the root, one in a directory that does not exist, one in a nested repository
/// (another top level), one that git ignores, and a `root` that is no repository are outside.
pub(crate) fn inside_git_tree(git: &Git, root: &Path, rel: &str) -> Result<bool, Refusal> {
    let rel_path = Path::new(rel);
    if rel.is_empty()
        || rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Ok(false);
    }
    // `Some(code)` when git ended normally; a stop is an error, a signal or timeout is a failure.
    let exit = |args: &[&str]| -> Result<(i32, String), Refusal> {
        let out = git.output(true, args)?;
        match out.end {
            End::Exited(n) => Ok((
                n,
                String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
            )),
            End::Stopped(r) => Err(r),
            End::Signaled => Err(failed("git was killed")),
            End::TimedOut => Err(failed("git timed out")),
        }
    };
    let top = |dir: &Path| -> Result<Option<String>, Refusal> {
        let (code, top) = exit(&["-C", text(dir)?, "rev-parse", "--show-toplevel"])?;
        Ok((code == 0).then_some(top))
    };
    let Some(root_top) = top(root)? else {
        return Ok(false);
    };
    // A link at the path may point outside the tree, whatever git tracks.
    let full = root.join(rel_path);
    if fs::symlink_metadata(&full).is_ok_and(|m| m.file_type().is_symlink()) {
        return Ok(false);
    }
    let Some(parent) = full.parent() else {
        return Ok(false);
    };
    if top(parent)?.as_deref() != Some(root_top.as_str()) {
        return Ok(false);
    }
    // `check-ignore -q`: 0 ignored, 1 not ignored; a tracked path is never reported as ignored.
    let (code, _) = exit(&["-C", text(root)?, "check-ignore", "-q", "--", rel])?;
    Ok(code == 1)
}

// ---- the command ------------------------------------------------------------------------------

/// The most candidates one run adopts. The journal intent lists each one (docs/adopt.md).
const MAX_CANDIDATES: usize = 64;
/// The journal bounds a token at 4096 bytes (`state::MAX_ERROR_BYTES`).
const MAX_TOKEN: usize = 4096;

#[derive(Debug)]
pub struct Args {
    pub trial: String,
    pub module: String,
    pub root: Option<PathBuf>,
    pub preview: bool,
    pub yes_plan: Option<String>,
}

/// `adopt <trial-id> --module <module> [--root <dir>] [--preview] [-y|--yes-plan <digest>]`.
pub fn parse_args(args: &[String]) -> Result<Args, Refusal> {
    let (mut trial, mut module, mut root, mut yes, mut preview) = (None, None, None, None, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |slot: &mut Option<String>| -> Result<(), Refusal> {
            let v = it
                .next()
                .ok_or_else(|| usage(format!("{a} needs a value")))?;
            if slot.replace(v.clone()).is_some() {
                return Err(usage(format!("{a} given twice")));
            }
            Ok(())
        };
        match a.as_str() {
            "--module" => value(&mut module)?,
            "--root" => value(&mut root)?,
            "--yes-plan" | "-y" => value(&mut yes)?,
            "--preview" if !preview => preview = true,
            "--preview" => return Err(usage("--preview given twice")),
            other if other.starts_with('-') => {
                return Err(usage(format!("unknown argument {other}")));
            }
            _ if trial.is_some() => return Err(usage("adopt takes one trial id")),
            _ => trial = Some(a.clone()),
        }
    }
    if preview && yes.is_some() {
        return Err(usage("--preview and --yes-plan cannot be used together"));
    }
    if yes.as_deref().is_some_and(|d| !is_digest(d)) {
        return Err(usage(
            "--yes-plan takes the 64 hex characters of a plan digest",
        ));
    }
    let trial = trial.ok_or_else(|| usage("adopt needs a trial id"))?;
    let module = module.ok_or_else(|| usage("adopt needs --module <module>"))?;
    for (what, v) in [("the trial id", &trial), ("--module", &module)] {
        if !valid_id(v) {
            return Err(usage(format!(
                "{what} must be lowercase ASCII letters, digits, '.', '_' or '-' (1 to 64 characters)"
            )));
        }
    }
    Ok(Args {
        trial,
        module,
        root: root.map(PathBuf::from),
        preview,
        yes_plan: yes,
    })
}

fn conflict(detail: impl Into<String>) -> Refusal {
    Refusal::new(Code::Conflict, detail)
}

/// One live file the trial created or changed, as adopt read it once.
#[derive(Clone)]
struct Candidate {
    /// The absolute live path.
    live: String,
    /// The path relative to HOME: the link target.
    rel: String,
    bytes: Vec<u8>,
    mode: u32,
    sha256: String,
    /// The repo copy, relative to the root.
    repo: String,
    /// The link source, relative to the module manifest.
    source: String,
    id: String,
    /// The baseline entry was a directory that existed before the trial.
    preexisting_dir: bool,
}

impl Candidate {
    fn approved(&self) -> ApprovedFile {
        ApprovedFile {
            fingerprint: file_fingerprint(&self.bytes),
            content_id: format!("sha256:{}", self.sha256),
        }
    }
}

/// A repo file adopt appends to: its path relative to the root, the hash of its bytes now, and
/// the text it appends (empty: none).
struct Edit {
    path: String,
    pre: String,
    post: String,
    addition: String,
}

/// What the preview shows and the digest covers.
struct Decision {
    /// The canonical root. Not in the digest: the paths below are relative to it.
    root: PathBuf,
    trial_id: String,
    /// SHA-256 of the trial record's canonical JSON: a changed record makes the approval stale.
    record: String,
    module: String,
    profile: String,
    manifest: Edit,
    /// The Brewfile adopt edits; `None` when the trial has no `homebrew/v1` package.
    brewfile: Option<Edit>,
    /// Why no Brewfile line is written, for the preview.
    brew_note: Option<String>,
    candidates: Vec<Candidate>,
    unknown: Vec<String>,
    binary: Binary,
    stamp: Stamp,
}

impl Decision {
    fn digest(&self) -> Result<String, Refusal> {
        let files: Vec<_> = self
            .candidates
            .iter()
            .map(|c| {
                json!({"path": c.live, "size": c.bytes.len(), "mode": c.mode, "sha256": c.sha256,
                    "repo": c.repo, "id": c.id})
            })
            .collect();
        let doc = json!({
            "trial_id": self.trial_id,
            "record": self.record,
            "module": self.module,
            "profile": self.profile,
            "manifest": {"path": self.manifest.path, "sha256": self.manifest.pre},
            "brewfile": self.brewfile.as_ref().map(|b| json!({"path": b.path, "sha256": b.pre})),
            "brew_line": self.brewfile.as_ref().map(|b| &b.addition),
            "candidates": files,
            "binary": {"version": self.binary.version, "sha256": self.binary.sha256},
            "binding": {
                "profile": self.stamp.profile,
                "machine_id": self.stamp.machine_id,
                "binding_digest": self.stamp.binding_digest,
            },
        });
        serde_json::to_vec(&doc)
            .map(|b| sha256_hex(&b))
            .map_err(|e| Refusal::new(Code::IoFailed, format!("cannot digest the adoption: {e}")))
    }

    /// What a later run needs to recognise this run's partial work by content: the module, the
    /// repo paths with the hash each copy holds, and each appended file with its hash before
    /// and after the append. It is the intent entry's `token`.
    fn token(&self) -> Result<String, Refusal> {
        let edit = |e: &Edit| json!({"path": e.path, "pre": e.pre, "post": e.post});
        let files: Vec<_> = self
            .candidates
            .iter()
            .map(|c| json!({"target": c.rel, "repo": c.repo, "sha256": c.sha256}))
            .collect();
        let doc = json!({
            "module": self.module,
            "manifest": edit(&self.manifest),
            "brewfile": self.brewfile.as_ref().filter(|b| !b.addition.is_empty()).map(edit),
            "files": files,
        });
        let t = serde_json::to_string(&doc).map_err(|e| {
            Refusal::new(Code::IoFailed, format!("cannot record the adoption: {e}"))
        })?;
        if t.len() > MAX_TOKEN {
            return Err(Refusal::new(
                Code::LimitExceeded,
                format!(
                    "the adoption record is {} bytes, over the journal's {MAX_TOKEN}; nothing was changed. Fix: shorten the candidate paths",
                    t.len()
                ),
            ));
        }
        Ok(t)
    }

    fn print(&self, out: &mut dyn Write, digest: &str) -> std::io::Result<()> {
        writeln!(out, "adopt: {} module={}", self.trial_id, self.module)?;
        writeln!(
            out,
            "manifest: {} (+{} fs-link/v1 resources, appended)",
            visible(&self.manifest.path),
            self.candidates.len()
        )?;
        for c in &self.candidates {
            let note = if c.preexisting_dir {
                " (pre-existing directory)"
            } else {
                ""
            };
            writeln!(
                out,
                "candidate: {} -> {} id={}{note}",
                visible(&c.live),
                visible(&c.repo),
                c.id
            )?;
        }
        match (&self.brewfile, &self.brew_note) {
            (Some(b), _) if !b.addition.is_empty() => writeln!(
                out,
                "brewfile: {} += {}",
                visible(&b.path),
                visible(b.addition.trim_end())
            )?,
            (_, Some(n)) => writeln!(out, "brewfile: none ({n})")?,
            _ => writeln!(out, "brewfile: none")?,
        }
        writeln!(
            out,
            "takeover: each live file becomes a link to its repo copy; nothing is committed"
        )?;
        if !self.unknown.is_empty() {
            writeln!(
                out,
                "unknown: {} (not adopted; review them by hand)",
                visible(&self.unknown.join(" "))
            )?;
        }
        writeln!(out, "plan digest: {digest}")
    }
}

/// The trial record `trial_id`, read under whatever lock `store` holds. Missing, unreadable and
/// adopted records are refused.
fn trial_record(store: &Store, trial_id: &str) -> Result<TrialRecord, Refusal> {
    let file = format!("{trial_id}.json");
    let t = match store.load().trials.into_iter().find(|(n, _)| *n == file) {
        Some((_, Ok(t))) if t.trial_id == trial_id => t,
        Some((_, Err(e))) => {
            return Err(Refusal::new(
                Code::StateInvalid,
                format!("trial {trial_id} cannot be read: {}", e.detail),
            ));
        }
        _ => {
            return Err(Refusal::new(
                Code::StateInvalid,
                format!("trial {trial_id} has no record in state/trials/"),
            ));
        }
    };
    if !t.is_active() {
        return Err(conflict(format!(
            "trial {trial_id} is already adopted; nothing was changed"
        )));
    }
    Ok(t)
}

/// Whether one path equals the other or lies inside it, as the file system compares names.
fn overlaps(a: &str, b: &str) -> bool {
    let (a, b) = (fold::fold(a), fold::fold(b));
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

/// Reads one candidate: a regular file not over the store's 16 MiB cap, without following a
/// link. Anything else refuses the run.
fn read_candidate(path: &Path) -> Result<(Vec<u8>, u32), Refusal> {
    let shown = path.display().to_string();
    let bad = |why: &str| conflict(format!("{shown} {why}; adopt takes regular files only"));
    let meta =
        fs::symlink_metadata(path).map_err(|e| bad(&format!("cannot be read ({})", e.kind())))?;
    if !meta.is_file() {
        return Err(bad("is a link or a special file"));
    }
    let limit = u64::try_from(MAX_BYTES).unwrap_or(u64::MAX);
    let bytes = read_regular_bounded(
        path,
        limit,
        |e| bad(&format!("cannot be read ({})", e.kind())),
        || bad("changed type while it was read"),
    )?;
    if bytes.len() > MAX_BYTES {
        return Err(bad("is larger than 16 MiB"));
    }
    // The permission bits only: a set-id or sticky bit is not copied into the checkout.
    Ok((bytes, meta.mode() & 0o777))
}

/// Every regular file below `dir`, sorted; a link or a special file refuses the run.
fn files_below(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Refusal> {
    let shown = dir.display().to_string();
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .and_then(|d| d.map(|e| e.map(|e| e.path())).collect())
        .map_err(|e| conflict(format!("{shown} cannot be listed ({})", e.kind())))?;
    entries.sort();
    for p in entries {
        let m = fs::symlink_metadata(&p)
            .map_err(|e| conflict(format!("{} cannot be read ({})", p.display(), e.kind())))?;
        if m.is_dir() {
            files_below(&p, out)?;
        } else if m.is_file() {
            out.push(p);
        } else {
            return Err(conflict(format!(
                "{} is a link or a special file; adopt takes regular files only",
                p.display()
            )));
        }
        if out.len() > MAX_CANDIDATES {
            return Err(Refusal::new(
                Code::LimitExceeded,
                format!("more than {MAX_CANDIDATES} files to adopt; nothing was changed"),
            ));
        }
    }
    Ok(())
}

/// One live file as adopt read it: path, bytes, permission bits, and whether it came from a
/// directory that existed before the trial.
type LiveFile = (PathBuf, Vec<u8>, u32, bool);

/// The live files the trial created or changed (docs/adopt.md, "Candidates"). Each file is read
/// once: the read that decides whether a pre-existing file changed also supplies its bytes.
fn live_candidates(t: &TrialRecord) -> Result<Vec<LiveFile>, Refusal> {
    let mut out = Vec::new();
    for b in &t.baseline.paths {
        let p = Path::new(&b.path);
        let now = match fs::symlink_metadata(p) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(conflict(format!(
                    "{} cannot be read ({})",
                    b.path,
                    e.kind()
                )));
            }
        };
        let Some(now) = now else { continue };
        let pre = b.existed && b.sha256.is_none();
        match (b.existed, &b.sha256) {
            (true, Some(was)) => {
                let (bytes, mode) = read_candidate(p)?;
                if file_fingerprint(&bytes) != *was {
                    out.push((p.to_path_buf(), bytes, mode, pre));
                }
            }
            _ if now.is_dir() => {
                let mut found = Vec::new();
                files_below(p, &mut found)?;
                for f in found {
                    let (bytes, mode) = read_candidate(&f)?;
                    out.push((f, bytes, mode, pre));
                }
            }
            _ => {
                let (bytes, mode) = read_candidate(p)?;
                out.push((p.to_path_buf(), bytes, mode, pre));
            }
        }
    }
    Ok(out)
}

/// Whether the current user may write to a file or directory with this metadata.
/// shortcut: group bits are not read, upgrade if a shared-group checkout must be supported.
fn writable(m: &fs::Metadata, uid: u32) -> bool {
    if m.uid() == uid {
        m.mode() & 0o200 != 0
    } else {
        m.mode() & 0o002 != 0
    }
}

/// Walks `rel` below `root` without following a link: no existing component may be a link, and
/// the deepest existing one (the file itself, or the directory the write creates entries in)
/// must be writable.
fn check_path(root: &Path, rel: &str, uid: u32) -> Result<(), Refusal> {
    let mut cur = root.to_path_buf();
    let mut deepest = fs::symlink_metadata(&cur)
        .map_err(|e| conflict(format!("{} cannot be read ({})", cur.display(), e.kind())))?;
    let mut at = cur.clone();
    for c in Path::new(rel).components() {
        let Component::Normal(name) = c else {
            return Err(conflict(format!("{rel} is not a plain relative path")));
        };
        cur.push(name);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(conflict(format!(
                    "{} is a link; adopt writes into the checkout only through real directories",
                    cur.display()
                )));
            }
            Ok(m) => {
                deepest = m;
                at = cur.clone();
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => {
                return Err(conflict(format!(
                    "{} cannot be read ({})",
                    cur.display(),
                    e.kind()
                )));
            }
        }
    }
    if !writable(&deepest, uid) {
        return Err(conflict(format!(
            "{} is not writable; nothing was changed",
            at.display()
        )));
    }
    Ok(())
}

/// The relative path of `p` below `base`, as text.
fn rel_text(p: &Path, base: &Path) -> Option<String> {
    p.strip_prefix(base).ok()?.to_str().map(str::to_string)
}

/// The parent directory of a root-relative path, empty for a file at the root.
fn dir_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(d, _)| d)
}

/// Gathers everything the run needs and refuses before any write (docs/adopt.md, "Order of one
/// run"). `scan` runs gitleaks; the decision under the lock skips it, because the digest covers
/// the hash of every scanned byte.
fn decide(
    ctx: &Ctx,
    args: &Args,
    store: &Store,
    (runner, git, uid): (&Runner, &Git, u32),
    (binary, stamp): (&Binary, &Stamp),
    scan_now: bool,
) -> Result<Decision, Refusal> {
    let trial = trial_record(store, &args.trial)?;
    // The record is canonical JSON on disk; its serialization here is the same document.
    let record = serde_json::to_vec(&trial)
        .map(|b| sha256_hex(&b))
        .map_err(|e| {
            Refusal::new(
                Code::IoFailed,
                format!("cannot digest the trial record: {e}"),
            )
        })?;
    let loaded = load_resolved(ctx, args.root.as_deref())?;
    let root = loaded.root.clone();
    let profile = rel_text(Path::new(loaded.binding.profile()), &root).ok_or_else(|| {
        Refusal::new(
            Code::ProfileInvalid,
            "the bound profile is not inside the root",
        )
    })?;
    let module = args.module.as_str();
    let modules_line = match loaded.sources.get(&profile) {
        Some(Entry::File(b)) => String::from_utf8_lossy(b)
            .lines()
            .find(|l| l.trim_start().starts_with("modules"))
            .map_or_else(|| "modules = [...]".to_string(), |l| l.trim().to_string()),
        _ => "modules = [...]".to_string(),
    };
    let Some(manifest) =
        module_manifest(&loaded.sources, &profile, module).map_err(Refusal::from)?
    else {
        return Err(Refusal::new(
            Code::ReferenceInvalid,
            format!(
                "the bundle of {profile} does not declare a module {module}; nothing was changed. Fix: name a declared module, or declare it in the bundle first (adopt never creates a module)"
            ),
        ));
    };
    if !loaded.resolved.modules.iter().any(|m| m == module) {
        return Err(Refusal::new(
            Code::ReferenceInvalid,
            format!(
                "the profile {profile} does not select module {module}; nothing was changed. Fix: add it to the line {modules_line} in {profile}, then run adopt again"
            ),
        ));
    }
    // C1: the manifest is the file the resolver read as a regular source file, not a lexical
    // guess; the resolve of the edited composition below checks that it declares the module.
    let Some(Entry::File(manifest_bytes)) = loaded.sources.get(&manifest) else {
        return Err(conflict(format!(
            "the manifest {manifest} of module {module} is not a regular file in the sources"
        )));
    };
    if !inside_git_tree(git, &root, &manifest)? {
        return Err(conflict(format!(
            "{manifest} is not inside the git checkout at {}; adopt edits only that checkout",
            root.display()
        )));
    }

    // The candidates, each read once.
    let home = ctx.layout().home;
    let module_dir = dir_of(&manifest);
    let mut candidates: Vec<Candidate> = Vec::new();
    for (path, bytes, mode, preexisting_dir) in live_candidates(&trial)? {
        let rel = rel_text(&path, &home)
            .ok_or_else(|| conflict(format!("{} is outside HOME", path.display())))?;
        let source = format!("home/{rel}");
        let repo = if module_dir.is_empty() {
            source.clone()
        } else {
            format!("{module_dir}/{source}")
        };
        candidates.push(Candidate {
            live: path.display().to_string(),
            id: resource_id(&rel),
            sha256: sha256_hex(&bytes),
            rel,
            bytes,
            mode,
            repo,
            source,
            preexisting_dir,
        });
    }
    if candidates.is_empty() {
        return Err(conflict(format!(
            "nothing to adopt: no file was created or changed under the baseline paths of trial {}",
            args.trial
        )));
    }

    // Ownership and collisions.
    let mut ids: BTreeMap<&str, &str> = BTreeMap::new();
    for c in &candidates {
        if let Some(other) = ids.insert(&c.id, &c.rel) {
            return Err(conflict(format!(
                "{other} and {} both give the resource id {}; nothing was changed. Fix: rename one of them",
                c.rel, c.id
            )));
        }
        for r in loaded.resolved.resources.values() {
            let dirs = r.dir_claims.iter().map(|d| &d.dir);
            if r.claims.iter().chain(dirs).any(|p| overlaps(p, &c.rel)) {
                return Err(conflict(format!(
                    "{} is already claimed by resource {} in module {}; adopt never overrides another resource",
                    c.rel, r.id, r.module
                )));
            }
            // A resolved id is `<module>.<id>`; an id may not repeat in any module.
            if r.id.strip_prefix(&format!("{}.", r.module)) == Some(c.id.as_str()) {
                return Err(conflict(format!(
                    "the resource id {} already exists in module {}",
                    c.id, r.module
                )));
            }
        }
        if fs::symlink_metadata(root.join(&c.repo)).is_ok() {
            return Err(conflict(format!(
                "{} already exists in the root; adopt never overwrites a repo file",
                c.repo
            )));
        }
    }
    for (_, t) in store.load().trials {
        let Ok(t) = t else { continue };
        if !t.is_active() || t.trial_id == args.trial {
            continue;
        }
        if let Some(c) = candidates
            .iter()
            .find(|c| t.owned_outputs.iter().any(|o| overlaps(o, &c.live)))
        {
            return Err(conflict(format!(
                "{} is claimed by the active trial {}",
                c.live, t.trial_id
            )));
        }
    }

    // The Brewfile, only for a homebrew/v1 trial.
    let (mut brewfile, mut brew_note) = (None, None);
    let mut brew_bytes = Vec::new();
    match &trial.package {
        Some(p) if p.adapter == HOMEBREW => {
            let own = format!("Brewfile.{module}");
            let path = if fs::symlink_metadata(root.join(&own)).is_ok() {
                own
            } else {
                "Brewfile".to_string()
            };
            let full = root.join(&path);
            brew_bytes = read_regular_bounded(
                &full,
                u64::try_from(MAX_BYTES).unwrap_or(u64::MAX),
                |e| {
                    conflict(format!(
                        "{} cannot be read ({}); adopt appends the package line there",
                        full.display(),
                        e.kind()
                    ))
                },
                || conflict(format!("{} is not a regular file", full.display())),
            )?;
            if brew_bytes.len() > MAX_BYTES {
                return Err(conflict(format!("{path} is larger than 16 MiB")));
            }
            if !inside_git_tree(git, &root, &path)? {
                return Err(conflict(format!(
                    "{path} is not inside the git checkout at {}, or git ignores it",
                    root.display()
                )));
            }
            brewfile = Some(Edit {
                pre: sha256_hex(&brew_bytes),
                post: String::new(),
                addition: String::new(),
                path,
            });
        }
        Some(p) => {
            brew_note = Some(format!(
                "the trial package is {}; only a homebrew/v1 package gets a Brewfile line",
                p.adapter
            ));
        }
        None => brew_note = Some("the trial recorded no provider package".into()),
    }

    // The checkout: clean edits, writable paths, no link on the way.
    let mut edited: Vec<&str> = vec![&manifest];
    edited.extend(brewfile.as_ref().map(|b| b.path.as_str()));
    let dirty_paths = dirty(git, &root, &edited)?;
    if !dirty_paths.is_empty() {
        return Err(conflict(format!(
            "{} has uncommitted changes; nothing was changed. Fix: commit or stash them, so adopt's edit is its own diff",
            dirty_paths.join(", ")
        )));
    }
    for rel in edited
        .iter()
        .copied()
        .chain(candidates.iter().map(|c| c.repo.as_str()))
    {
        check_path(&root, rel, uid)?;
    }

    if scan_now {
        let files: Vec<(String, Vec<u8>)> = candidates
            .iter()
            .map(|c| (c.live.clone(), c.bytes.clone()))
            .collect();
        let hits = scan(runner, &ctx.platform.cache_dir().join("adopt-scan"), &files)?;
        if !hits.is_empty() {
            return Err(conflict(format!(
                "gitleaks found a possible secret in {}; nothing was changed. Fix: move the secret into a 1Password template by hand, then run adopt again",
                visible(&hits.join(", "))
            )));
        }
    }

    // The Brewfile line: the package as a formula, or as a cask when brew holds it as one. A
    // line for any spelling of the same package counts as present.
    if let (Some(b), Some(p)) = (brewfile.as_mut(), &trial.package) {
        let (name, spellings) = package::brewfile_names(&p.name);
        let text = String::from_utf8_lossy(&brew_bytes);
        if spellings.iter().any(|n| brewfile_has(&text, n)) {
            brew_note = Some(format!("{} already lists {name}", b.path));
        } else {
            let kind = if package::brew_is_cask(runner, &p.name)? {
                BrewKind::Cask
            } else {
                BrewKind::Formula
            };
            b.addition = brewfile_line(kind, &name);
        }
        b.post = sha256_hex(&append(&brew_bytes, &b.addition));
    }

    // The edited composition must resolve, and plan each new link as an approved takeover.
    let blocks: String = candidates
        .iter()
        .map(|c| link_block(&c.id, &c.rel, &c.source))
        .collect();
    let new_manifest = append(manifest_bytes, &blocks);
    let mut sources = loaded.sources.clone();
    sources.insert_file(manifest.clone(), new_manifest.clone());
    for c in &candidates {
        sources.insert_file(c.repo.clone(), c.bytes.clone());
    }
    let resolved = resolve(&sources, &profile, &crate::builtin_catalog()).map_err(|e| {
        let r = Refusal::from(e);
        Refusal::new(
            r.code,
            format!("the composition with the adopted resources does not resolve: {}; nothing was changed", r.detail),
        )
    })?;
    for c in &candidates {
        let ok = resolved.resources.values().any(|r| {
            r.id == format!("{module}.{}", c.id)
                && r.module == module
                && r.declared_in == manifest
                && r.target.as_deref() == Some(c.rel.as_str())
        });
        if !ok {
            return Err(conflict(format!(
                "the resource {} does not resolve from {manifest} in module {module}",
                c.id
            )));
        }
    }
    let edited_loaded = Loaded {
        binding: loaded.binding.clone(),
        root: root.clone(),
        staged: None,
        links: BTreeMap::new(),
        generated: BTreeMap::new(),
        sources,
        resolved,
    };
    takeover_plan(ctx, &edited_loaded, module, &candidates)?;

    Ok(Decision {
        root,
        trial_id: args.trial.clone(),
        record,
        module: module.to_string(),
        profile,
        manifest: Edit {
            path: manifest.clone(),
            pre: sha256_hex(manifest_bytes),
            post: sha256_hex(&new_manifest),
            addition: blocks,
        },
        brewfile,
        brew_note,
        candidates,
        unknown: trial.unknown.clone(),
        binary: binary.clone(),
        stamp: stamp.clone(),
    })
}

/// The configuration plan of `loaded` with the candidates approved for takeover, cut to the
/// operations of the new resources. Each must be a planned `adopt-file` with nothing to run
/// first, and no blocker may name one; the rest of the composition is not touched.
fn takeover_plan(
    ctx: &Ctx,
    loaded: &Loaded,
    module: &str,
    candidates: &[Candidate],
) -> Result<Plan, Refusal> {
    let adopt_files = candidates
        .iter()
        .map(|c| (c.rel.clone(), c.approved()))
        .collect();
    let probed = Probed {
        adopt_files,
        ..Probed::default()
    };
    let (mut plan, _) = plan_with_outputs(ctx, loaded, Scope::Config, probed)?;
    let gid = |c: &Candidate| format!("{module}.{}", c.id);
    let ids: BTreeSet<String> = candidates.iter().map(gid).collect();
    plan.blockers.retain(|b| ids.contains(b.resource.as_str()));
    if let Some(b) = plan.blockers.first() {
        return Err(b.refusal());
    }
    plan.operations
        .retain(|o| ids.contains(o.resource.as_str()));
    for c in candidates {
        let ok = plan
            .operations
            .iter()
            .filter(|o| o.resource == gid(c))
            .all(|o| {
                o.action == Action::AdoptFile
                    && o.status == PlanStatus::Planned
                    && o.after.is_empty()
            });
        let n = plan
            .operations
            .iter()
            .filter(|o| o.resource == gid(c))
            .count();
        if !ok || n != 1 {
            return Err(conflict(format!(
                "the plan does not replace {} with its link as one approved takeover; nothing more was changed",
                c.live
            )));
        }
    }
    Ok(plan)
}

/// Creates the repo copy `rel` below `root` exclusively, with `mode`, creating missing parent
/// directories. A link anywhere on the way refuses the write; nothing is followed.
fn create_repo_file(root: &Path, rel: &str, bytes: &[u8], mode: u32) -> Result<(), Refusal> {
    let io = |what: &str, p: &Path, e: std::io::Error| {
        Refusal::new(
            Code::IoFailed,
            format!("{what} {}: {}", p.display(), e.kind()),
        )
    };
    let mut cur = root.to_path_buf();
    let parent = Path::new(rel).parent().unwrap_or(Path::new(""));
    for c in parent.components() {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(conflict(format!(
                    "{} is not a directory (a link or a file)",
                    cur.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                DirBuilder::new()
                    .create(&cur)
                    .map_err(|e| io("create", &cur, e))?;
            }
            Err(e) => return Err(io("read", &cur, e)),
        }
    }
    let path = root.join(rel);
    // `create_new` is O_CREAT|O_EXCL, which never follows a link at the name.
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW)
        .open(&path)
        .map_err(|e| io("create", &path, e))?;
    f.set_permissions(fs::Permissions::from_mode(mode))
        .and_then(|()| f.write_all(bytes))
        .and_then(|()| f.sync_all())
        .map_err(|e| io("write", &path, e))
}

/// Appends `addition` to the repo file `rel`. The file is opened once without following a link,
/// its bytes are read from that descriptor and must still hash to `pre`, and the append goes
/// through the same descriptor, so a file swapped in after the check is never written.
fn append_checked(root: &Path, rel: &str, pre: &str, addition: &str) -> Result<(), Refusal> {
    let path = root.join(rel);
    let io = |e: std::io::Error| {
        Refusal::new(
            Code::IoFailed,
            format!("append to {}: {}", path.display(), e.kind()),
        )
    };
    let mut f = OpenOptions::new()
        .read(true)
        .append(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(&path)
        .map_err(io)?;
    if !f.metadata().map_err(io)?.is_file() {
        return Err(conflict(format!("{rel} is not a regular file")));
    }
    let mut bytes = Vec::new();
    (&mut f)
        .take(u64::try_from(MAX_BYTES).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if sha256_hex(&bytes) != pre {
        return Err(Refusal::new(
            Code::PlanStale,
            format!("{rel} changed after it was checked; it was not edited"),
        ));
    }
    let tail = &append(&bytes, addition)[bytes.len()..];
    f.write_all(tail).and_then(|()| f.sync_all()).map_err(io)
}

pub fn run(ctx: &Ctx, args: &Args, out: &mut dyn Write) -> Result<(), Refusal> {
    let w =
        |e: std::io::Error| Refusal::new(Code::IoFailed, format!("output failed: {}", e.kind()));
    let binding = require_binding(ctx)?;
    let binary = binary_identity()?;
    let stamp = Stamp::of(&binding);
    // gitleaks, git and brew run before anything is decided, so a stop request ends the child.
    signal::install()?;
    let stop = || signal::requested().map_or(Ok(()), Err);
    let (layout, sys) = (ctx.layout(), ctx.sys()?);
    let runner = ctx.runner(&sys);
    let git = Git::new(ctx, &sys);
    let tools = (&runner, &git, sys.uid);
    let fail = state_cmd::fail_from_env()?;
    let readable =
        Store::open_read(sys, &layout, fail.clone())?.ok_or_else(crate::state_lost_refusal)?;
    readable.require_identity(&binding)?;
    let first = decide(ctx, args, &readable, tools, (&binary, &stamp), true)?;
    stop()?;
    let digest = first.digest()?;
    if let Some(i) = &ctx.isolated {
        writeln!(out, "isolated: true {i}").map_err(w)?;
    }
    first.print(out, &digest).map_err(w)?;
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
                "nothing was changed. Fix: read the adoption above, then run again with --yes-plan {digest}"
            ),
        ));
    };
    drop(readable);
    fail.hit("adopt-before-lock")?;

    // Take the lock, then decide again: the live files, the checkout or the record may have
    // changed since the preview.
    let store = Store::open_write_with(sys, &layout, fail.clone(), false)?;
    if !store.is_initialized() {
        return Err(store.stamp(crate::state_lost_refusal()));
    }
    store
        .require_identity(&binding)
        .map_err(|r| store.stamp(r))?;
    store.read_ledger().map_err(|r| store.stamp(r))?;
    let locked =
        decide(ctx, args, &store, tools, (&binary, &stamp), false).map_err(|r| store.stamp(r))?;
    stop().map_err(|r| store.stamp(r))?;
    let locked_digest = locked.digest().map_err(|r| store.stamp(r))?;
    if locked_digest != digest {
        return Err(store.stamp(stale(approved, &locked_digest)));
    }
    let token = locked.token().map_err(|r| store.stamp(r))?;
    let started = now_ns().map_err(|r| store.stamp(r))?;
    let op_id = format!("adopt-{started}-{}", std::process::id());
    let root = &locked.root;
    let journal = |phase, recovery, token, reason| -> Result<(), Refusal> {
        store.append_journal(&JournalEntry {
            schema: SCHEMA,
            op_id: op_id.clone(),
            // Not `adopt`: that name is the engine's link adoption, which recovery probes.
            op: "trial-adopt".into(),
            phase,
            at: rfc3339(now_ns()?),
            resource: args.trial.clone(),
            adapter: "fs-link/v1".into(),
            class: Class::Normal,
            recovery,
            token,
            reason,
        })
    };
    // The intent names no output path: the repo lies outside HOME, and a path would let the
    // engine's settling take this entry for a link adoption. The token holds the record.
    journal(
        Phase::Intent,
        Some(Recovery {
            outputs: vec![],
            snapshot: None,
        }),
        Some(token),
        None,
    )
    .map_err(|r| store.stamp(r))?;

    // From here on every outcome ends in a receipt and a result line, and a refusal exits 1.
    let mut ops: Vec<OperationRecord> = Vec::new();
    let mut changed: Vec<String> = Vec::new();
    let edits = (|| -> Result<(), Refusal> {
        for c in &locked.candidates {
            let at = now_ns()?;
            create_repo_file(root, &c.repo, &c.bytes, c.mode)?;
            ops.push(written(&op_id, ops.len(), &c.repo, None, &c.sha256, at));
            changed.push(c.repo.clone());
        }
        fail.hit("adopt-after-copy")?;
        let edits = [Some(&locked.manifest), locked.brewfile.as_ref()];
        for (e, point) in edits
            .into_iter()
            .zip(["adopt-after-manifest", "adopt-after-brewfile"])
        {
            if let Some(e) = e.filter(|e| !e.addition.is_empty()) {
                let at = now_ns()?;
                append_checked(root, &e.path, &e.pre, &e.addition)?;
                ops.push(written(
                    &op_id,
                    ops.len(),
                    &e.path,
                    Some(&e.pre),
                    &e.post,
                    at,
                ));
                changed.push(e.path.clone());
            }
            fail.hit(point)?;
        }
        // The repo edits are done; the takeover below is journaled by the engine per link, so
        // this entry closes before `apply` settles the journal.
        journal(Phase::Done, None, None, None)
    })();
    let mut stopped: Option<Refusal> = None;
    match edits {
        Ok(()) => {}
        Err(e) => {
            let why = bounded_visible(&e.detail, 1024);
            let e = match journal(Phase::Failed, None, None, Some(why)) {
                Ok(()) => e,
                Err(n) => Refusal::new(
                    e.code,
                    format!(
                        "{}; the outcome could not be journaled: {}",
                        e.detail, n.detail
                    ),
                ),
            };
            stopped = Some(e);
        }
    }

    // The takeover: the engine replaces each live file with its link, under this lock.
    let mut approvals = Vec::new();
    if stopped.is_none() {
        let mut takeover = || -> Result<(), Refusal> {
            let loaded = load_resolved(ctx, Some(root))?;
            let plan = takeover_plan(ctx, &loaded, &args.module, &locked.candidates)?;
            let allow_adopt: BTreeSet<String> =
                plan.operations.iter().map(|o| o.id.clone()).collect();
            let home = Dir::open_home(&sys, &layout.home)?;
            let generator = crate::adapters::Generators::new(&loaded.resolved);
            let run = Run {
                store: &store,
                sys,
                home: &home,
                home_path: home.shown(),
                generator: &generator,
                allow: &BTreeSet::new(),
                allow_adopt: &allow_adopt,
                fail: &fail,
            };
            let report = dfm_engine::apply(&run, &plan)?;
            for (op, rec) in plan.operations.iter().zip(&report.operations) {
                if Allowance::of(op) == Some(Allowance::Adopt) && rec.id != op.journal_id {
                    approvals.push(DisruptiveApproval {
                        operation_id: rec.id.clone(),
                        how: Allowance::Adopt.how().into(),
                        at: rec.started_at.clone().unwrap_or_else(|| rfc3339(started)),
                    });
                }
            }
            ops.extend(report.operations);
            if let Some(r) = report.stopped {
                return Err(r);
            }
            fail.hit("adopt-after-link")?;
            store.mark_trial_adopted(
                &args.trial,
                TrialAdopted {
                    op_id: op_id.clone(),
                    module: args.module.clone(),
                    at_unix_ns: now_ns()?,
                },
            )
        };
        stopped = takeover().err();
    }
    let stopped = signal::requested().or(stopped).map(Refusal::after_mutation);
    let finished = now_ns().unwrap_or(started);
    let exit = stopped.as_ref().map_or(Exit::Ok, Refusal::exit);
    ops.push(OperationRecord {
        id: op_id.clone(),
        resource: args.trial.clone(),
        adapter: "adopt".into(),
        class: Class::Normal,
        status: if stopped.is_some() {
            Status::Failed
        } else {
            Status::Updated
        },
        started_at: Some(rfc3339(started)),
        finished_at: Some(rfc3339(finished)),
        before: None,
        after: None,
        owned_outputs: vec![],
        error: stopped.as_ref().map(|e| bounded_visible(&e.detail, 1024)),
        reload: None,
    });
    let receipt = Receipt {
        schema: SCHEMA,
        op_id: op_id.clone(),
        command: "adopt".into(),
        update: false,
        started_at: rfc3339(started),
        finished_at: rfc3339(finished),
        binary: BinaryId {
            version: binary.version,
            commit: option_env!("DFM_BUILD_COMMIT").unwrap_or("unknown").into(),
            sha256: binary.sha256,
        },
        profile: ProfileRef {
            id: stamp.profile,
            binding_digest: stamp.binding_digest,
        },
        machine_id: stamp.machine_id,
        plan_digest: Some(digest.clone()),
        approval: Some(Approval {
            kind: "yes_plan".into(),
            plan_digest: digest,
            at: rfc3339(started),
        }),
        sources: vec![],
        approvals,
        result: RunResult {
            outcome: exit.name().into(),
            exit_code: exit.code(),
            counts: count_statuses(&ops),
        },
        operations: ops,
    };
    let receipt_name = store.write_receipt(&receipt);
    for o in &receipt.operations {
        let error = o
            .error
            .as_deref()
            .map_or(String::new(), |e| format!(": {e}"));
        writeln!(
            out,
            "  {} {} {}{error}",
            label(o.status),
            visible(&o.resource),
            o.id
        )
        .map_err(w)?;
    }
    for p in &changed {
        writeln!(out, "changed: {}", visible(p)).map_err(w)?;
    }
    if stopped.is_none() {
        writeln!(
            out,
            "trial: state/trials/{}.json adopted into {}",
            args.trial, args.module
        )
        .map_err(w)?;
    }
    let outcome = match (receipt_name, stopped) {
        (Ok(name), stopped) => {
            writeln!(out, "receipt: {}", visible(&name)).map_err(w)?;
            stopped.map_or(Ok(()), Err)
        }
        (Err(e), stopped) => {
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

/// The receipt operation for one written repo path: the hash before an append (none for a new
/// file) and after.
fn written(
    op_id: &str,
    n: usize,
    path: &str,
    before: Option<&str>,
    after: &str,
    at: u64,
) -> OperationRecord {
    OperationRecord {
        id: format!("{op_id}-{n}"),
        resource: path.into(),
        adapter: "adopt".into(),
        class: Class::Normal,
        status: Status::Updated,
        started_at: Some(rfc3339(at)),
        finished_at: Some(rfc3339(now_ns().unwrap_or(at))),
        before: before.map(|f| Fingerprint {
            fingerprint: f.into(),
        }),
        after: Some(Fingerprint {
            fingerprint: after.into(),
        }),
        owned_outputs: vec![],
        error: None,
        reload: None,
    }
}

// The fake `gitleaks` of the integration tests, shared with the unit tests below.
#[cfg(test)]
#[path = "../tests/common/providers.rs"]
#[expect(dead_code, reason = "the unit tests use only the gitleaks look-alike")]
mod providers;

#[cfg(test)]
mod tests {
    use super::providers::FAKE_GITLEAKS;
    use super::*;
    use crate::Ctx;
    use dfm_engine::Sys;
    use dfm_platform_macos::Platform;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    fn scratch(label: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "dfm-adopt-{label}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn uid() -> u32 {
        dfm_platform_macos::current_uid().unwrap()
    }

    fn runner(search: Vec<PathBuf>, base: &Path) -> Runner {
        Runner {
            search_path: search,
            home: base.join("home"),
            cwd: base.join("run"),
            uid: uid(),
            grace: Duration::from_millis(200),
            detach: false,
        }
    }

    /// A fixture with `bin/gitleaks` as the look-alike; `log` and markers sit beside `bin`.
    fn fake(base: &Path) -> Runner {
        fs::create_dir_all(base.join("bin")).unwrap();
        let exe = base.join("bin/gitleaks");
        fs::write(&exe, FAKE_GITLEAKS).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        runner(vec![base.join("bin")], base)
    }

    fn files(items: &[(&str, &str)]) -> Vec<(String, Vec<u8>)> {
        items
            .iter()
            .map(|(p, c)| (p.to_string(), c.as_bytes().to_vec()))
            .collect()
    }

    fn leftovers(parent: &Path) -> usize {
        fs::read_dir(parent).map_or(0, |d| d.count())
    }

    #[test]
    fn arguments_take_one_trial_id_and_a_required_module() {
        let a = |v: &[&str]| parse_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let ok = a(&["trial-x", "--module", "core", "--root", "/r", "--preview"]).unwrap();
        assert_eq!(
            (ok.trial.as_str(), ok.module.as_str(), ok.preview),
            ("trial-x", "core", true)
        );
        assert_eq!(ok.root.as_deref(), Some(Path::new("/r")));
        let d = "0".repeat(64);
        assert_eq!(
            a(&["t", "--module", "m", "-y", &d])
                .unwrap()
                .yes_plan
                .as_deref(),
            Some(d.as_str())
        );
        for bad in [
            vec![],
            vec!["t"],
            vec!["t", "u", "--module", "m"],
            vec!["t", "--module"],
            vec!["t", "--module", "m", "--module", "n"],
            vec!["t", "--module", "M"],
            vec!["T", "--module", "m"],
            vec!["t", "--module", "m", "--preview", "-y", &d],
            vec!["t", "--module", "m", "-y", "zz"],
            vec!["t", "--module", "m", "--force"],
        ] {
            assert_eq!(a(&bad).unwrap_err().code, Code::Usage, "{bad:?}");
        }
    }

    #[test]
    fn the_digest_covers_the_files_the_edits_the_binary_and_the_binding() {
        let edit = |pre: &str| Edit {
            path: "m/module.toml".into(),
            pre: pre.into(),
            post: "p".into(),
            addition: "a".into(),
        };
        let cand = |sha: &str, mode: u32| Candidate {
            live: "/h/.config/t/c".into(),
            rel: ".config/t/c".into(),
            bytes: b"x".to_vec(),
            mode,
            sha256: sha.into(),
            repo: "m/home/.config/t/c".into(),
            source: "home/.config/t/c".into(),
            id: "config-t-c".into(),
            preexisting_dir: false,
        };
        let dr =
            |pre: &str, brew: Option<&str>, sha: &str, mode: u32, machine: &str, record: &str| {
                Decision {
                    root: PathBuf::from("/r"),
                    trial_id: "trial-t".into(),
                    record: record.into(),
                    module: "core".into(),
                    profile: "p.toml".into(),
                    manifest: edit(pre),
                    brewfile: brew.map(|line| Edit {
                        path: "Brewfile".into(),
                        pre: "b".into(),
                        post: "b2".into(),
                        addition: line.into(),
                    }),
                    brew_note: None,
                    candidates: vec![cand(sha, mode)],
                    unknown: vec![],
                    binary: Binary {
                        version: "0".into(),
                        sha256: "s".into(),
                    },
                    stamp: Stamp {
                        profile: "p".into(),
                        machine_id: machine.into(),
                        binding_digest: "d".into(),
                    },
                }
                .digest()
                .unwrap()
            };
        let d = |pre: &str, brew: Option<&str>, sha: &str, mode: u32, machine: &str| {
            dr(pre, brew, sha, mode, machine, "t1")
        };
        let base = d("m1", Some("brew \"t\"\n"), "c1", 0o644, "m");
        assert_eq!(base, d("m1", Some("brew \"t\"\n"), "c1", 0o644, "m"));
        assert_ne!(base, d("m2", Some("brew \"t\"\n"), "c1", 0o644, "m"));
        assert_ne!(base, d("m1", Some("cask \"t\"\n"), "c1", 0o644, "m"));
        assert_ne!(base, d("m1", None, "c1", 0o644, "m"));
        assert_ne!(base, d("m1", Some("brew \"t\"\n"), "c2", 0o644, "m"));
        assert_ne!(base, d("m1", Some("brew \"t\"\n"), "c1", 0o600, "m"));
        assert_ne!(base, d("m1", Some("brew \"t\"\n"), "c1", 0o644, "n"));
        // A changed trial record changes the digest.
        assert_ne!(base, dr("m1", Some("brew \"t\"\n"), "c1", 0o644, "m", "t2"));
    }

    #[test]
    fn paths_overlap_when_equal_or_nested_ignoring_case() {
        assert!(overlaps(".config/t", ".config/t/c"));
        assert!(overlaps(".config/t/c", ".config/t"));
        assert!(overlaps(".Config/T/c", ".config/t/c"));
        assert!(!overlaps(".config/t", ".config/tt"));
    }

    #[test]
    fn scan_maps_findings_back_to_candidate_paths() {
        let base = scratch("scan");
        let r = fake(&base);
        let parent = base.join("adopt-scan");
        let input = files(&[
            ("/h/.config/a", "nothing here"),
            ("/h/.config/b", "x FAKE_SECRET y"),
            ("/h/c", "FAKE_SECRET"),
        ]);
        let hits = scan(&r, &parent, &input).unwrap();
        assert_eq!(hits, ["/h/.config/b", "/h/c"]);
        assert_eq!(
            scan(&r, &parent, &input[..1]).unwrap(),
            Vec::<String>::new()
        );
        let log = fs::read_to_string(base.join("log")).unwrap();
        assert!(log.contains("--redact"), "{log}");
        assert!(log.contains("--report-path -"), "{log}");
        assert!(!log.contains("--config"), "{log}");
        assert!(log.contains("--gitleaks-ignore-path"), "{log}");
        // The scanned directory was private while the scan ran, and nothing is left after.
        assert!(log.lines().any(|l| l == "drwx------"), "{log}");
        assert_eq!(leftovers(&parent), 0);
    }

    #[test]
    fn scan_refuses_when_gitleaks_is_missing_or_fails() {
        let base = scratch("fail");
        let parent = base.join("adopt-scan");
        let input = files(&[("/h/a", "x")]);
        // Missing: the search path holds no gitleaks.
        fs::create_dir_all(base.join("empty")).unwrap();
        let missing = runner(vec![base.join("empty")], &base);
        let e = scan(&missing, &parent, &input).unwrap_err();
        assert_eq!(e.code, Code::ProbeFailed);
        assert_eq!(leftovers(&parent), 0);
        // Failed: exit 1. The tool's stderr does not reach the refusal.
        let r = fake(&base);
        fs::write(base.join("gitleaks-fail"), "").unwrap();
        let e = scan(&r, &parent, &input).unwrap_err();
        assert_eq!(e.code, Code::ProbeFailed);
        assert!(!e.detail.contains("PLANTED-STDERR"), "{}", e.detail);
        assert_eq!(leftovers(&parent), 0);
    }

    #[test]
    fn a_report_that_cannot_be_mapped_is_a_failed_scan() {
        let names = ["/h/a".to_string()];
        let ok = |j: &str| findings(j.as_bytes(), Path::new("/s/files"), &names);
        assert_eq!(ok(r#"[{"File":"/s/files/0/a"}]"#).unwrap(), ["/h/a"]);
        for bad in [
            "",
            "not json",
            "[]",
            r#"{"File":"/s/files/0"}"#,
            r#"[{"Other":1}]"#,
            r#"[{"File":"/elsewhere/0"}]"#,
            r#"[{"File":"/s/files/9"}]"#,
        ] {
            assert_eq!(ok(bad).unwrap_err().code, Code::ProbeFailed, "{bad:?}");
        }
    }

    #[test]
    fn a_gitleaks_config_in_the_candidate_directory_is_not_used() {
        // Local proof only: hosted CI does not install gitleaks, so this test skips there.
        let found = std::env::var_os("PATH")
            .and_then(|p| std::env::split_paths(&p).find(|d| d.join("gitleaks").is_file()));
        let Some(real) = found else {
            eprintln!("skipped: no gitleaks on PATH");
            return;
        };
        let base = scratch("real");
        let r = runner(vec![real], &base);
        let parent = base.join("adopt-scan");
        // The key is the AWS documentation example with its last character changed, so the
        // default `aws-access-token` rule matches it and a repository scanner does not flag it.
        let key = "aws_access_key_id = AKIAIOSFODNN7EXAMPLQ\n";
        let input = files(&[
            (
                "/h/.gitleaks.toml",
                "[allowlist]\nregexes = [\".*\"]\npaths = [\".*\"]\n",
            ),
            (
                "/h/.gitleaksignore",
                "0:aws-access-token:1\n1:aws-access-token:1\n",
            ),
            ("/h/.config/tool/credentials", key),
            // An inline allow comment and a path-only rule (pkcs12-file) must still report.
            (
                "/h/.config/tool/commented",
                "aws_access_key_id = AKIAIOSFODNN7EXAMPLQ # gitleaks:allow\n",
            ),
            ("/h/.config/tool/cert.p12", "not really a key store"),
            // Names the default path allowlist skips: the content must still be scanned.
            ("/h/.config/tool/x.svg", key),
            ("/h/package-lock.json", key),
            ("/h/other/.gitleaks.toml", key),
        ]);
        let hits = scan(&r, &parent, &input).unwrap();
        assert_eq!(
            hits,
            [
                "/h/.config/tool/cert.p12",
                "/h/.config/tool/commented",
                "/h/.config/tool/credentials",
                "/h/.config/tool/x.svg",
                "/h/other/.gitleaks.toml",
                "/h/package-lock.json"
            ]
        );
        assert_eq!(leftovers(&parent), 0);
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let out = Command::new("/usr/bin/git")
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn git_for(home: &Path) -> Git {
        let ctx = Ctx {
            platform: Platform::with_home(home.to_path_buf()),
            binding_path: home.join("binding.toml"),
            os: "macos".into(),
            arch: "aarch64".into(),
            isolated: None,
            color: false,
        };
        Git::new(&ctx, &Sys { uid: uid() })
    }

    /// A repository with committed `modules/a/module.toml` and `modules/b/module.toml`.
    fn repo(base: &Path) -> PathBuf {
        let root = base.join("repo");
        fs::create_dir_all(root.join("modules/a")).unwrap();
        fs::create_dir_all(root.join("modules/b")).unwrap();
        git_in(&root, &["init", "-q", "."]);
        fs::write(root.join("modules/a/module.toml"), "a\n").unwrap();
        fs::write(root.join("modules/b/module.toml"), "b\n").unwrap();
        git_in(&root, &["add", "."]);
        git_in(&root, &["commit", "-q", "-m", "init"]);
        root
    }

    #[test]
    fn dirty_names_modified_and_untracked_paths_only() {
        let base = scratch("dirty");
        let root = repo(&base);
        let git = git_for(&base.join("home"));
        let all = ["modules/a/module.toml", "modules/b/module.toml", "Brewfile"];
        assert!(dirty(&git, &root, &all).unwrap().is_empty());
        assert!(dirty(&git, &root, &[]).unwrap().is_empty());
        fs::write(root.join("modules/a/module.toml"), "changed\n").unwrap();
        fs::write(root.join("Brewfile"), "brew \"x\"\n").unwrap();
        assert_eq!(
            dirty(&git, &root, &all).unwrap(),
            ["Brewfile", "modules/a/module.toml"]
        );
        // A path outside the list is not reported.
        assert_eq!(
            dirty(&git, &root, &["modules/a/module.toml"]).unwrap(),
            ["modules/a/module.toml"]
        );
        // Not a repository: the check cannot answer.
        let plain = base.join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert_eq!(
            dirty(&git, &plain, &["x"]).unwrap_err().code,
            Code::ProbeFailed
        );
    }

    #[test]
    fn a_tracked_or_untracked_path_of_the_same_checkout_is_inside_it() {
        let base = scratch("inside");
        let root = repo(&base);
        let git = git_for(&base.join("home"));
        assert!(inside_git_tree(&git, &root, "modules/a/module.toml").unwrap());
        // Untracked, in a directory of the same checkout.
        fs::create_dir_all(root.join("modules/new")).unwrap();
        assert!(inside_git_tree(&git, &root, "modules/new/module.toml").unwrap());
        // A directory that does not exist yet, a path that leaves the root, a path of a nested
        // repository, and a root that is no repository are all outside.
        assert!(!inside_git_tree(&git, &root, "nowhere/module.toml").unwrap());
        assert!(!inside_git_tree(&git, &root, "../repo/modules/a/module.toml").unwrap());
        assert!(!inside_git_tree(&git, &root, "/etc/hosts").unwrap());
        // A tracked link to a file outside the tree is not inside it.
        std::os::unix::fs::symlink("/etc/hosts", root.join("modules/a/link.toml")).unwrap();
        git_in(&root, &["add", "."]);
        git_in(&root, &["commit", "-q", "-m", "link"]);
        assert!(!inside_git_tree(&git, &root, "modules/a/link.toml").unwrap());
        let nested = root.join("vendor/inner");
        fs::create_dir_all(&nested).unwrap();
        git_in(&nested, &["init", "-q", "."]);
        fs::write(nested.join("module.toml"), "n\n").unwrap();
        assert!(!inside_git_tree(&git, &root, "vendor/inner/module.toml").unwrap());
        let plain = base.join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert!(!inside_git_tree(&git, &plain, "module.toml").unwrap());
    }
}
