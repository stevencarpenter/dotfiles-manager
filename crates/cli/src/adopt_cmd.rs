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
    Action, ApprovedFile, Binary, MAX_SOURCE_ENTRIES, MAX_SOURCE_FILE_BYTES,
    MAX_SOURCE_TOTAL_BYTES, Plan, Scope, Status as PlanStatus, file_fingerprint,
};
use dfm_model::resolve::{Entry, SourceSet, module_manifest, resolve};
use dfm_model::state::{
    Approval, BinaryId, Class, DisruptiveApproval, Fingerprint, JournalEntry, MAX_BYTES,
    OperationRecord, Phase, ProfileRef, Receipt, Recovery, RunResult, SCHEMA, Stamp, Status,
    TrialAdopted, TrialRecord, count_statuses, rfc3339, unfinished, unresolved, valid_id,
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
/// The longest wait for the lock on `adopt-scan/`, which a scan holds for milliseconds.
const SCAN_LOCK_WAIT: Duration = Duration::from_secs(10);
const MAX_REPORT: usize = 16 << 20;
/// gitleaks' own "leaks found" status, set with `--exit-code`: no other end can mean a finding.
const LEAKS: i32 = 3;

fn failed(detail: impl Into<String>) -> Refusal {
    Refusal::new(Code::ProbeFailed, detail)
}

fn io_failed(what: &str, e: &std::io::Error) -> Refusal {
    Refusal::new(Code::IoFailed, format!("{what}: {}", e.kind()))
}

/// A path as text for a tool's arguments. A non-UTF-8 path blocks the run before any write.
fn text(p: &Path) -> Result<&str, Refusal> {
    p.to_str().ok_or_else(|| {
        conflict(format!(
            "{} is not a UTF-8 path; adopt passes paths to its tools as text, and nothing was changed",
            p.display()
        ))
    })
}

/// Removes the scan directory on every exit, an error and a stop included. A failure to remove
/// is dropped: the scan result decides the run. A SIGKILL leaves the directory behind, 0700 with
/// 0600 files under the manager's cache directory, until the next scan removes it
/// ([`sweep_dead`]). The lock file inside it is released by the kernel when the process ends.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The lock file a scan holds in its own directory while it runs.
const SCAN_LOCK: &str = "lock";

/// Removes the `<pid>-<ns>` scan directories in `scan_dir` that no running scan holds, which a
/// SIGKILL leaves behind (`Cleanup` never ran). The caller holds the lock on `scan_dir`, under
/// which every scan creates its directory and locks its `lock` file. So a directory whose
/// `lock` can be locked here belongs to no live process; no signal is sent and a reused pid does
/// not matter. A directory without a `lock` is a scan killed before it locked, or a scan of an
/// older binary that makes none (during a self-update), so it is removed only once its `<ns>` is
/// older than [`SCAN_TIMEOUT`]. Only a real directory of `uid` is removed, never through a
/// link. A failure is dropped, as in `Cleanup`.
fn sweep_dead(scan_dir: &Path, uid: u32, now: u64) {
    let young = |ns: u64| u128::from(now.saturating_sub(ns)) < SCAN_TIMEOUT.as_nanos();
    let Ok(entries) = fs::read_dir(scan_dir) else {
        return;
    };
    for e in entries.flatten() {
        let Some(ns) = e
            .file_name()
            .to_str()
            .and_then(|n| n.split_once('-'))
            .filter(|(pid, _)| pid.parse::<u32>().is_ok())
            .and_then(|(_, ns)| ns.parse::<u64>().ok())
        else {
            continue;
        };
        let path = e.path();
        if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir() && m.uid() == uid) {
            continue;
        }
        let held = match OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(path.join(SCAN_LOCK))
        {
            Ok(f) => f.try_lock().is_err(),
            Err(_) => young(ns),
        };
        if !held {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

/// Takes the lock on `scan_dir`, which another scan holds only while it sweeps and makes its
/// own directory. As for the writer lock, nothing blocks in `flock`: the lock is tried, and
/// tried again every 10 ms for at most [`SCAN_LOCK_WAIT`]. A stop request ends the wait with
/// its own refusal (exit 130 or 143), an `EINTR` is tried again, and a lock still held at the
/// end is `E_LOCKED`.
fn lock_scan_dir(held: &fs::File, scan_dir: &Path) -> Result<(), Refusal> {
    let until = std::time::Instant::now() + SCAN_LOCK_WAIT;
    loop {
        if let Some(stop) = signal::requested() {
            return Err(stop);
        }
        match held.try_lock() {
            Ok(()) => return Ok(()),
            Err(fs::TryLockError::WouldBlock) if std::time::Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(fs::TryLockError::WouldBlock) => {
                return Err(Refusal::new(
                    Code::Locked,
                    format!(
                        "another adopt scan holds {}; nothing was changed",
                        scan_dir.display()
                    ),
                ));
            }
            Err(fs::TryLockError::Error(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(fs::TryLockError::Error(e)) => {
                return Err(io_failed(&format!("lock {}", scan_dir.display()), &e));
            }
        }
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
    // Checked before anything is created: every path below is passed to gitleaks as text.
    text(scan_dir)?;
    private(scan_dir, true)?;
    // The directory is opened without following a link and checked through that descriptor,
    // which then holds the lock under which dead scans are swept and this scan's directory is
    // made and locked.
    let foreign = || {
        conflict(format!(
            "{} is not a directory of yours; nothing was changed",
            scan_dir.display()
        ))
    };
    if fs::symlink_metadata(scan_dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(foreign());
    }
    let held = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(scan_dir)
        .map_err(|e| io_failed(&format!("open {}", scan_dir.display()), &e))?;
    let parent = held
        .metadata()
        .map_err(|e| io_failed(&format!("read {}", scan_dir.display()), &e))?;
    if !parent.is_dir() || parent.uid() != runner.uid {
        return Err(foreign());
    }
    if parent.mode() & 0o7777 != 0o700 {
        fs::set_permissions(scan_dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| io_failed(&format!("protect {}", scan_dir.display()), &e))?;
    }
    lock_scan_dir(&held, scan_dir)?;
    sweep_dead(scan_dir, runner.uid, now_ns()?);
    let run = scan_dir.join(format!("{}-{}", std::process::id(), now_ns()?));
    private(&run, false)?;
    let _cleanup = Cleanup(run.clone());
    let lock = run.join(SCAN_LOCK);
    let _mine = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&lock)
        .and_then(|f| f.try_lock().map(|()| f).map_err(std::io::Error::other))
        .map_err(|e| io_failed(&format!("lock {}", lock.display()), &e))?;
    drop(held);
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
    let out = runner
        .run(&ProcRequest {
            program: "gitleaks",
            args: &args,
            env: &[],
            stdin: None,
            timeout: SCAN_TIMEOUT,
            max_stdout: MAX_REPORT,
        })
        .map_err(|e| {
            // A report that does not fit cannot be read as clean.
            if e.code == Code::LimitExceeded {
                failed("the gitleaks report is larger than 16 MiB")
            } else {
                e
            }
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
    let top = |dir: &Path| -> Result<Option<String>, Refusal> {
        let (code, top) = git_exit(git, &["-C", text(dir)?, "rev-parse", "--show-toplevel"])?;
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
    Ok(check_ignore(git, root, rel)? == 1)
}

/// Runs git with `args`: the exit code and the trimmed standard output when git ended normally.
/// A stop is an error, a signal or timeout is a failure.
fn git_exit(git: &Git, args: &[&str]) -> Result<(i32, String), Refusal> {
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
}

/// `git check-ignore -q` for `rel` under `root`: 0 ignored, 1 not ignored, anything else a git
/// error. A tracked path is never reported as ignored; the path need not exist.
fn check_ignore(git: &Git, root: &Path, rel: &str) -> Result<i32, Refusal> {
    git_exit(git, &["-C", text(root)?, "check-ignore", "-q", "--", rel]).map(|(code, _)| code)
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
    /// An interrupted run of this adoption already wrote the repo copy.
    copied: bool,
    /// The live path already is the planned link to the repo copy; `bytes` were read from it.
    linked: bool,
}

impl Candidate {
    fn approved(&self) -> ApprovedFile {
        ApprovedFile {
            fingerprint: file_fingerprint(&self.bytes),
            content_id: format!("sha256:{}", self.sha256),
        }
    }
}

/// A repo file adopt appends to: its path relative to the root, the hash of its bytes before the
/// append, and the text it appends (empty: none). `done`: an interrupted run already appended it.
struct Edit {
    path: String,
    pre: String,
    post: String,
    addition: String,
    done: bool,
}

/// An appended file as the `trial-adopt` token records it.
struct PriorEdit {
    path: String,
    pre: String,
    post: String,
}

/// The token of the latest `trial-adopt` intent of this trial and module, in any phase: what an
/// interrupted run planned, so this run can recognise that run's work by content.
struct Prior {
    op_id: String,
    manifest: PriorEdit,
    brewfile: Option<PriorEdit>,
    /// `(target, repo path)` to the SHA-256 of the copy.
    files: BTreeMap<(String, String), String>,
}

/// The latest `trial-adopt` intent of `trial` into `module`. An intent from before the trial
/// began belongs to an earlier trial of the same id, and one for another module is not this
/// adoption; both are ignored. A token that cannot be read refuses the run: recovery never
/// guesses.
fn prior_run(store: &Store, trial: &TrialRecord, module: &str) -> Result<Option<Prior>, Refusal> {
    let entries = store.read_journal_lenient()?;
    let Some(e) = entries.iter().rev().find(|e| {
        e.op == "trial-adopt" && e.resource == trial.trial_id && e.phase == Phase::Intent
    }) else {
        return Ok(None);
    };
    if !began_in(e, trial) {
        return Ok(None);
    }
    let bad = || {
        Refusal::new(
            Code::StateInvalid,
            format!(
                "the journal intent {} of trial {} cannot be read; nothing was changed",
                e.op_id, trial.trial_id
            ),
        )
    };
    let v: serde_json::Value =
        serde_json::from_str(e.token.as_deref().ok_or_else(bad)?).map_err(|_| bad())?;
    if v["module"].as_str() != Some(module) {
        return Ok(None);
    }
    let s = |v: &serde_json::Value| v.as_str().map(str::to_string).ok_or_else(bad);
    let edit = |v: &serde_json::Value| -> Result<PriorEdit, Refusal> {
        Ok(PriorEdit {
            path: s(&v["path"])?,
            pre: s(&v["pre"])?,
            post: s(&v["post"])?,
        })
    };
    let brewfile = match &v["brewfile"] {
        serde_json::Value::Null => None,
        b => Some(edit(b)?),
    };
    let mut files = BTreeMap::new();
    for f in v["files"].as_array().ok_or_else(bad)? {
        files.insert((s(&f["target"])?, s(&f["repo"])?), s(&f["sha256"])?);
    }
    Ok(Some(Prior {
        op_id: e.op_id.clone(),
        manifest: edit(&v["manifest"])?,
        brewfile,
        files,
    }))
}

/// Whether `e` started after `trial` was created. The op id is `adopt-<start ns>-<pid>`.
fn began_in(e: &JournalEntry, trial: &TrialRecord) -> bool {
    e.op_id
        .strip_prefix("adopt-")
        .and_then(|r| r.split_once('-'))
        .and_then(|(ns, _)| ns.parse::<u64>().ok())
        .is_some_and(|ns| ns >= trial.created_unix_ns)
}

/// Closes the open `trial-adopt` intents of earlier runs of this adoption (`trial` into
/// `module`) as `failed`, because the run `op_id` completes their work (docs/adopt.md,
/// "Interruption"). They name no output path, so settling could never decide them. The close
/// claims nothing and runs nothing again. An intent for another module or with an unreadable
/// token stays open for the operator.
fn close_superseded(
    store: &Store,
    trial: &TrialRecord,
    module: &str,
    op_id: &str,
) -> Result<(), Refusal> {
    let entries = store.read_journal_lenient()?;
    let module_of = |e: &JournalEntry| {
        serde_json::from_str::<serde_json::Value>(e.token.as_deref()?).ok()?["module"]
            .as_str()
            .map(str::to_string)
    };
    let open = unfinished(&entries)
        .into_iter()
        .chain(unresolved(&entries).into_iter().filter_map(|u| u.intent))
        .filter(|e| {
            e.op == "trial-adopt"
                && e.resource == trial.trial_id
                && e.op_id != op_id
                && began_in(e, trial)
                && module_of(e).as_deref() == Some(module)
        });
    for e in open {
        store.append_journal(&JournalEntry {
            phase: Phase::Failed,
            at: rfc3339(now_ns()?),
            recovery: None,
            token: None,
            reason: Some(format!("superseded by {op_id}")),
            ..e.clone()
        })?;
    }
    Ok(())
}

/// `current` without the interrupted run's append: `Some` only when `current` hashes to the
/// recorded `post` and is exactly bytes hashing to the recorded `pre` plus `addition`, as
/// `append` writes it (with the newline it adds to a file that lacks one).
fn prior_base<'a>(current: &'a [u8], addition: &str, prior: &PriorEdit) -> Option<&'a [u8]> {
    if addition.is_empty() || sha256_hex(current) != prior.post {
        return None;
    }
    let base = current.strip_suffix(addition.as_bytes())?;
    [base, base.strip_suffix(b"\n").unwrap_or(base)]
        .into_iter()
        .find(|b| sha256_hex(b) == prior.pre && append(b, addition) == current)
}

/// The refusal for a repo file that is neither as the interrupted run found it nor that plus
/// its planned append.
fn not_prior(path: &str, op_id: &str) -> Refusal {
    conflict(format!(
        "{path} is neither as the interrupted adopt {op_id} found it nor that plus its full planned append; nothing was changed. Fix: revert only what changed in it after the stop (a partial append at its end is such a change), then run adopt again"
    ))
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
    /// The op id of the interrupted run whose work this run completes.
    resume: Option<String>,
}

impl Decision {
    fn digest(&self) -> Result<String, Refusal> {
        let files: Vec<_> = self
            .candidates
            .iter()
            .map(|c| {
                json!({"path": c.live, "size": c.bytes.len(), "mode": c.mode, "sha256": c.sha256,
                    "repo": c.repo, "id": c.id, "copied": c.copied, "linked": c.linked})
            })
            .collect();
        let doc = json!({
            "trial_id": self.trial_id,
            "record": self.record,
            "module": self.module,
            "profile": self.profile,
            "manifest": {"path": self.manifest.path, "sha256": self.manifest.pre,
                "done": self.manifest.done},
            "brewfile": self.brewfile.as_ref()
                .map(|b| json!({"path": b.path, "sha256": b.pre, "done": b.done})),
            "resume": self.resume,
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
        let kept = |done: bool| {
            if done {
                " (kept from the interrupted run)"
            } else {
                ""
            }
        };
        writeln!(out, "adopt: {} module={}", self.trial_id, self.module)?;
        if let Some(op) = &self.resume {
            writeln!(
                out,
                "resume: {op} was interrupted; its own work below is kept, the rest runs now"
            )?;
        }
        writeln!(
            out,
            "manifest: {} (+{} fs-link/v1 resources, appended){}",
            visible(&self.manifest.path),
            self.candidates.len(),
            kept(self.manifest.done)
        )?;
        for c in &self.candidates {
            let mut note = String::new();
            if c.preexisting_dir {
                note.push_str(" (pre-existing directory)");
            }
            if c.linked {
                note.push_str(" (link kept from the interrupted run)");
            } else if c.copied {
                note.push_str(" (repo copy kept from the interrupted run)");
            }
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
                "brewfile: {} += {}{}",
                visible(&b.path),
                visible(b.addition.trim_end()),
                kept(b.done)
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

/// Live paths an interrupted run of this adoption planned to link, to their repo copies.
type Linked = BTreeMap<PathBuf, PathBuf>;

/// Reads one candidate. A live path that already is the link an interrupted run planned is
/// read through its repo copy (`true`); any other link refuses the run, as in `read_candidate`.
fn read_live(path: &Path, linked: &Linked) -> Result<(Vec<u8>, u32, bool), Refusal> {
    if let Some(repo) = linked.get(path)
        && fs::read_link(path).is_ok_and(|t| t == *repo)
    {
        let (bytes, mode) = read_candidate(repo)?;
        return Ok((bytes, mode, true));
    }
    read_candidate(path).map(|(bytes, mode)| (bytes, mode, false))
}

/// Every regular file below `dir`, sorted; a link or a special file refuses the run, except a
/// link in `linked`, which `read_live` checks.
fn files_below(dir: &Path, linked: &Linked, out: &mut Vec<PathBuf>) -> Result<(), Refusal> {
    let shown = dir.display().to_string();
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .and_then(|d| d.map(|e| e.map(|e| e.path())).collect())
        .map_err(|e| conflict(format!("{shown} cannot be listed ({})", e.kind())))?;
    entries.sort();
    for p in entries {
        let m = fs::symlink_metadata(&p)
            .map_err(|e| conflict(format!("{} cannot be read ({})", p.display(), e.kind())))?;
        if m.is_dir() {
            files_below(&p, linked, out)?;
        } else if m.is_file() || linked.contains_key(&p) {
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

/// One live file as adopt read it: path, bytes, permission bits, whether it came from a
/// directory that existed before the trial, and whether it already is the planned link.
type LiveFile = (PathBuf, Vec<u8>, u32, bool, bool);

/// The live files the trial created or changed (docs/adopt.md, "Candidates"). Each file is read
/// once: the read that decides whether a pre-existing file changed also supplies its bytes.
fn live_candidates(t: &TrialRecord, linked: &Linked) -> Result<Vec<LiveFile>, Refusal> {
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
                let (bytes, mode, link) = read_live(p, linked)?;
                if file_fingerprint(&bytes) != *was {
                    out.push((p.to_path_buf(), bytes, mode, pre, link));
                }
            }
            _ if now.is_dir() => {
                let mut found = Vec::new();
                files_below(p, linked, &mut found)?;
                for f in found {
                    let (bytes, mode, link) = read_live(&f, linked)?;
                    out.push((f, bytes, mode, pre, link));
                }
            }
            _ => {
                let (bytes, mode, link) = read_live(p, linked)?;
                out.push((p.to_path_buf(), bytes, mode, pre, link));
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

/// Walks `rel` below `root` without following a link: no existing component may be a link. With
/// `writes`, the deepest existing one (the file itself, or the directory the write creates
/// entries in) must also be writable; a file kept from an interrupted run is only read.
fn check_path(root: &Path, rel: &str, uid: u32, writes: bool) -> Result<(), Refusal> {
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
    if writes && !writable(&deepest, uid) {
        return Err(conflict(format!(
            "{} is not writable; nothing was changed",
            at.display()
        )));
    }
    Ok(())
}

/// The source loader's tree limits (`source_fs`) for the edited composition: entries (files,
/// links and walked directories, the new parent directories of the repo copies included) and
/// bytes in total, and the per-file limit for the appended manifest. A directory the loader
/// walks only to reach a `[sources]` root is not counted there; counting it here is stricter.
fn within_source_limits(sources: &SourceSet, candidates: &[Candidate]) -> Result<(), Refusal> {
    let limit = |what: String| {
        Refusal::new(
            Code::LimitExceeded,
            format!("{what}, over the source loader's limit; nothing was changed"),
        )
    };
    let dirs: BTreeSet<String> = sources.dirs().map(fold::fold).collect();
    let new_dirs: BTreeSet<String> = candidates
        .iter()
        .flat_map(|c| {
            c.repo
                .match_indices('/')
                .map(|(i, _)| fold::fold(&c.repo[..i]))
        })
        .filter(|d| !dirs.contains(d))
        .collect();
    let entries = sources.len() + dirs.len() + new_dirs.len();
    if entries > MAX_SOURCE_ENTRIES {
        return Err(limit(format!(
            "the checkout with the adopted files has {entries} source entries, more than {MAX_SOURCE_ENTRIES}"
        )));
    }
    let mut total: u64 = 0;
    for p in sources.paths() {
        let Some(Entry::File(b)) = sources.get(p) else {
            continue;
        };
        let n = u64::try_from(b.len()).unwrap_or(u64::MAX);
        if n > MAX_SOURCE_FILE_BYTES {
            return Err(limit(format!(
                "{p} would be {n} bytes, more than {MAX_SOURCE_FILE_BYTES}"
            )));
        }
        total = total.saturating_add(n);
    }
    if total > MAX_SOURCE_TOTAL_BYTES {
        return Err(limit(format!(
            "the checkout with the adopted files holds {total} source bytes, more than {MAX_SOURCE_TOTAL_BYTES}"
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

    // An interrupted run of this adoption (docs/adopt.md, "Interruption"): its own work is
    // recognised by content against its token, and anything else is refused.
    let prior = prior_run(store, &trial, module)?;
    let home = ctx.layout().home;
    let linked: Linked = prior
        .iter()
        .flat_map(|p| p.files.keys())
        .map(|(target, repo)| (home.join(target), root.join(repo)))
        .collect();

    // The candidates, each read once.
    let module_dir = dir_of(&manifest);
    let mut candidates: Vec<Candidate> = Vec::new();
    for (path, bytes, mode, preexisting_dir, linked) in live_candidates(&trial, &linked)? {
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
            copied: false,
            linked,
        });
    }
    if candidates.is_empty() {
        return Err(conflict(format!(
            "nothing to adopt: no file was created or changed under the baseline paths of trial {}",
            args.trial
        )));
    }
    // The source loader refuses these in the checkout. The edited composition below holds the
    // bytes in memory and would not, so the adoption would break every later `plan` and `sync`.
    for c in &candidates {
        if u64::try_from(c.bytes.len()).unwrap_or(u64::MAX) > MAX_SOURCE_FILE_BYTES {
            return Err(Refusal::new(
                Code::LimitExceeded,
                format!(
                    "{} is larger than {MAX_SOURCE_FILE_BYTES} bytes, the limit for a source file, so its repo copy {} would not load; nothing was changed. Fix: keep the file out of the module",
                    c.live, c.repo
                ),
            ));
        }
        if c.repo.split('/').any(|n| n.to_lowercase() == ".git") {
            return Err(Refusal::new(
                Code::SourceUntrusted,
                format!(
                    "{} would be copied to {}, under a nested .git, which the source loader refuses (submodules and gitlinks); nothing was changed. Fix: keep the file out of the module",
                    c.live, c.repo
                ),
            ));
        }
    }

    // A repo path that exists is refused, unless it is the interrupted run's copy: it holds the
    // bytes that run recorded, and those are still the candidate's bytes.
    // The walk refuses a link anywhere on the way, so a kept copy (or the copy a kept link was
    // read through) is never accepted when it is reached through a directory swapped for a link.
    for c in &mut candidates {
        check_path(&root, &c.repo, uid, false)?;
        // `git add` skips an ignored path, so a fresh checkout would miss the link source.
        match check_ignore(git, &root, &c.repo)? {
            1 => {}
            0 => {
                return Err(conflict(format!(
                    "git ignores the repo path {} in the checkout at {}, so the copy would never be committed and a fresh checkout could not resolve {}; nothing was changed. Fix: remove or narrow the ignore rule (`git check-ignore -v {}` names it), then run adopt again",
                    c.repo,
                    root.display(),
                    c.id,
                    c.repo
                )));
            }
            n => return Err(failed(format!("git check-ignore {} exited {n}", c.repo))),
        }
        let full = root.join(&c.repo);
        match fs::symlink_metadata(&full) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            _ => {}
        }
        let (Some(p), Some(sha)) = (
            prior.as_ref(),
            prior
                .as_ref()
                .and_then(|p| p.files.get(&(c.rel.clone(), c.repo.clone()))),
        ) else {
            return Err(conflict(format!(
                "{} already exists in the root; adopt never overwrites a repo file",
                c.repo
            )));
        };
        let copy_ok = read_candidate(&full).is_ok_and(|(b, _)| sha256_hex(&b) == *sha);
        let why = if c.linked && *sha != c.sha256 {
            Some(format!(
                "the repo copy {} changed after the takeover (a write through the link {}); nothing was changed. Fix: review the change; commit it or revert it in the copy, then run adopt again",
                c.repo, c.live
            ))
        } else if !copy_ok {
            Some(format!(
                "{} is not the copy the interrupted adopt {} wrote (changed, or cut short by the stop); nothing was changed. Fix: compare it with {}; if it is a partial copy, remove it, then run adopt again",
                c.repo, p.op_id, c.live
            ))
        } else if *sha != c.sha256 {
            Some(format!(
                "{} changed after the interrupted adopt {} copied it to {}; nothing was changed. Fix: restore the live file to the copied bytes, or remove the copy to adopt the new bytes, then run adopt again",
                c.live, p.op_id, c.repo
            ))
        } else {
            None
        };
        if let Some(why) = why {
            return Err(conflict(why));
        }
        c.copied = true;
    }

    // The manifest: as found, or as the interrupted run left it after its append. In that case
    // the checks below run against the composition before the append.
    let blocks: String = candidates
        .iter()
        .map(|c| link_block(&c.id, &c.rel, &c.source))
        .collect();
    let mut manifest_base: &[u8] = manifest_bytes;
    let mut manifest_done = false;
    if let Some(p) = prior.as_ref().filter(|p| p.manifest.path == manifest)
        && sha256_hex(manifest_bytes) != p.manifest.pre
    {
        manifest_base = prior_base(manifest_bytes, &blocks, &p.manifest)
            .ok_or_else(|| not_prior(&manifest, &p.op_id))?;
        manifest_done = true;
    }
    let base_resolved;
    let resolved_now = if manifest_done {
        let mut s = loaded.sources.clone();
        s.insert_file(manifest.clone(), manifest_base.to_vec());
        base_resolved = resolve(&s, &profile, &crate::builtin_catalog()).map_err(Refusal::from)?;
        &base_resolved
    } else {
        &loaded.resolved
    };

    // Ownership and collisions.
    let mut ids: BTreeMap<&str, &str> = BTreeMap::new();
    for c in &candidates {
        if let Some(other) = ids.insert(&c.id, &c.rel) {
            return Err(conflict(format!(
                "{other} and {} both give the resource id {}; nothing was changed. Fix: rename one of them",
                c.rel, c.id
            )));
        }
        for r in resolved_now.resources.values() {
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
    }
    // A ledger entry at the target, or above or below it, is another owner, even when no module
    // declares its resource now (an orphan). Only a kept link of the interrupted run may already
    // be recorded, and only for its own new resource.
    let ledger = store.read_ledger_lenient()?;
    for c in &candidates {
        let own = format!("{module}.{}", c.id);
        let owner = ledger.resources.iter().find(|r| {
            !(c.linked && r.resource == own)
                && r.owned_outputs.iter().any(|o| overlaps(&o.path, &c.live))
        });
        if let Some(r) = owner {
            return Err(conflict(format!(
                "{} is recorded in the ledger for resource {}; adopt never takes over an output the manager records, and nothing was changed",
                c.live, r.resource
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
                done: false,
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

    // The Brewfile line: the package as a formula, or as a cask when brew holds it as one. A
    // line of that kind for any spelling of the same package counts as present, unless the interrupted run
    // appended it: then the file must be its recorded bytes plus that line.
    if let (Some(b), Some(p)) = (brewfile.as_mut(), &trial.package) {
        let (name, spellings) = package::brewfile_names(&p.name);
        // The probe runs only when the kind decides something: the line to write, or which of
        // two kinds an existing line must be.
        let kind = || -> Result<BrewKind, Refusal> {
            Ok(if package::brew_is_cask(runner, &p.name)? {
                BrewKind::Cask
            } else {
                BrewKind::Formula
            })
        };
        let resumed = prior.as_ref().and_then(|r| {
            r.brewfile
                .as_ref()
                .filter(|e| e.path == b.path && e.pre != b.pre)
                .map(|e| (e, &r.op_id))
        });
        if let Some((e, op_id)) = resumed {
            let line = brewfile_line(kind()?, &name);
            brew_bytes = prior_base(&brew_bytes, &line, e)
                .ok_or_else(|| not_prior(&b.path, op_id))?
                .to_vec();
            b.pre.clone_from(&e.pre);
            b.addition = line;
            b.done = true;
        } else {
            let text = String::from_utf8_lossy(&brew_bytes);
            let listed = |k| spellings.iter().any(|n| brewfile_has(&text, k, n));
            let listed_as = match (listed(BrewKind::Formula), listed(BrewKind::Cask)) {
                (false, false) => None,
                (true, false) => Some(Some(BrewKind::Formula)),
                (false, true) => Some(Some(BrewKind::Cask)),
                // Both kinds are listed: whichever brew holds is there, so no probe runs.
                (true, true) => Some(None),
            };
            match listed_as {
                None => b.addition = brewfile_line(kind()?, &name),
                Some(None) => brew_note = Some(format!("{} already lists {name}", b.path)),
                Some(Some(have)) => match kind() {
                    Ok(k) if k != have => b.addition = brewfile_line(k, &name),
                    Ok(_) => brew_note = Some(format!("{} already lists {name}", b.path)),
                    // A stop still ends the run. Any other probe failure leaves the line the
                    // owner wrote: adopt only skips its own append.
                    Err(r) if matches!(r.code, Code::Interrupted | Code::Terminated) => {
                        return Err(r);
                    }
                    Err(_) => {
                        brew_note = Some(format!(
                            "{} already lists {name}; brew could not say whether it is a formula or a cask",
                            b.path
                        ));
                    }
                },
            }
        }
        b.post = sha256_hex(&append(&brew_bytes, &b.addition));
    }

    // The checkout: clean edits, writable paths, no link on the way. A file the interrupted run
    // already wrote is not written again, so it is not checked for changes or for writability;
    // the walk for links still covers it.
    let mut edited: Vec<&str> = Vec::new();
    if !manifest_done {
        edited.push(&manifest);
    }
    edited.extend(
        brewfile
            .as_ref()
            .filter(|b| !b.done)
            .map(|b| b.path.as_str()),
    );
    let dirty_paths = dirty(git, &root, &edited)?;
    if !dirty_paths.is_empty() {
        return Err(conflict(format!(
            "{} has uncommitted changes; nothing was changed. Fix: commit or stash them, so adopt's edit is its own diff",
            dirty_paths.join(", ")
        )));
    }
    for rel in edited.iter().copied().chain(
        candidates
            .iter()
            .filter(|c| !c.copied)
            .map(|c| c.repo.as_str()),
    ) {
        check_path(&root, rel, uid, true)?;
    }
    let kept_edits = [
        manifest_done.then_some(manifest.as_str()),
        brewfile
            .as_ref()
            .filter(|b| b.done)
            .map(|b| b.path.as_str()),
    ];
    for rel in kept_edits.into_iter().flatten() {
        check_path(&root, rel, uid, false)?;
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

    // The edited composition must resolve, and plan each new link as an approved takeover.
    let new_manifest = append(manifest_base, &blocks);
    let mut sources = loaded.sources.clone();
    sources.insert_file(manifest.clone(), new_manifest.clone());
    for c in &candidates {
        sources.insert_file(c.repo.clone(), c.bytes.clone());
    }
    within_source_limits(&sources, &candidates)?;
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
            pre: sha256_hex(manifest_base),
            post: sha256_hex(&new_manifest),
            addition: blocks,
            done: manifest_done,
        },
        brewfile,
        brew_note,
        candidates,
        unknown: trial.unknown.clone(),
        binary: binary.clone(),
        stamp: stamp.clone(),
        resume: prior.map(|p| p.op_id),
    })
}

/// The configuration plan of `loaded` with the candidates approved for takeover, cut to the
/// operations of the new resources. Each must be a planned `adopt-file` with nothing to run
/// first, and no blocker may name one; the rest of the composition is not touched. A candidate
/// whose live path already is its link (an interrupted run) is `current`, or a planned `adopt`
/// that records the owner when recovery closed the engine's intent without one.
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
                o.after.is_empty()
                    && match (o.action, o.status) {
                        (Action::AdoptFile, PlanStatus::Planned) => !c.linked,
                        (Action::Adopt, PlanStatus::Planned)
                        | (Action::None, PlanStatus::Current) => c.linked,
                        _ => false,
                    }
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
            changed.push(c.repo.clone());
            if c.copied {
                continue;
            }
            let at = now_ns()?;
            create_repo_file(root, &c.repo, &c.bytes, c.mode)?;
            ops.push(written(&op_id, ops.len(), &c.repo, None, &c.sha256, at));
        }
        fail.hit("adopt-after-copy")?;
        let edits = [Some(&locked.manifest), locked.brewfile.as_ref()];
        for (e, point) in edits
            .into_iter()
            .zip(["adopt-after-manifest", "adopt-after-brewfile"])
        {
            if let Some(e) = e.filter(|e| !e.addition.is_empty()) {
                changed.push(e.path.clone());
                if e.done {
                    fail.hit(point)?;
                    continue;
                }
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
            }
            fail.hit(point)?;
        }
        // The repo edits are done; the takeover below is journaled by the engine per link, so
        // these entries close before `apply` settles the journal.
        close_superseded(
            &store,
            &trial_record(&store, &args.trial)?,
            &args.module,
            &op_id,
        )?;
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
            // A link an interrupted run already owns plans `current` and needs no approval.
            let allow_adopt: BTreeSet<String> = plan
                .operations
                .iter()
                .filter(|o| o.status == PlanStatus::Planned)
                .map(|o| o.id.clone())
                .collect();
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
            done: false,
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
            copied: false,
            linked: false,
        };
        let decision =
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
                        done: false,
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
                    resume: None,
                }
            };
        let dr =
            |pre: &str, brew: Option<&str>, sha: &str, mode: u32, machine: &str, record: &str| {
                decision(pre, brew, sha, mode, machine, record)
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
        // What an interrupted run left, and which run it was, change the digest.
        let changed = |f: &dyn Fn(&mut Decision)| {
            let mut x = decision("m1", Some("brew \"t\"\n"), "c1", 0o644, "m", "t1");
            f(&mut x);
            x.digest().unwrap()
        };
        assert_eq!(base, changed(&|_| {}));
        assert_ne!(base, changed(&|x| x.resume = Some("adopt-1-2".into())));
        assert_ne!(base, changed(&|x| x.manifest.done = true));
        assert_ne!(base, changed(&|x| x.brewfile.as_mut().unwrap().done = true));
        assert_ne!(base, changed(&|x| x.candidates[0].copied = true));
        assert_ne!(base, changed(&|x| x.candidates[0].linked = true));
    }

    #[test]
    fn prior_base_accepts_only_the_recorded_bytes_plus_the_planned_append() {
        let rec = |pre: &[u8], add: &str| PriorEdit {
            path: "m".into(),
            pre: sha256_hex(pre),
            post: sha256_hex(&append(pre, add)),
        };
        let add = "[[resources]]\nid = \"x\"\n";
        for pre in [&b"a = 1\n"[..], b"a = 1", b""] {
            let r = rec(pre, add);
            let now = append(pre, add);
            assert_eq!(prior_base(&now, add, &r), Some(pre), "{pre:?}");
            // Not yet appended, edited after the append, or another append: not its work.
            assert_eq!(prior_base(pre, add, &r), None);
            let mut edited = now.clone();
            edited.extend_from_slice(b"# hand edit\n");
            assert_eq!(prior_base(&edited, add, &r), None);
            assert_eq!(prior_base(&now, "[[resources]]\nid = \"y\"\n", &r), None);
            assert_eq!(prior_base(&now, "", &r), None);
        }
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
    fn a_candidate_named_like_its_number_is_copied_once() {
        let base = scratch("base-i");
        let r = fake(&base);
        let parent = base.join("adopt-scan");
        let input = files(&[("/h/0", "FAKE_SECRET"), ("/h/x/1", "clean")]);
        assert_eq!(scan(&r, &parent, &input).unwrap(), ["/h/0"]);
        assert_eq!(leftovers(&parent), 0);
    }

    #[test]
    fn an_oversize_report_is_a_failed_scan() {
        let base = scratch("huge");
        let r = fake(&base);
        let parent = base.join("adopt-scan");
        fs::write(base.join("gitleaks-huge"), "").unwrap();
        let e = scan(&r, &parent, &files(&[("/h/a", "x")])).unwrap_err();
        assert_eq!(e.code, Code::ProbeFailed, "{e:?}");
        assert_eq!(leftovers(&parent), 0);
    }

    #[test]
    fn a_scan_directory_of_another_user_is_a_conflict() {
        let base = scratch("foreign");
        let mut r = fake(&base);
        r.uid += 1;
        let e = scan(&r, &base.join("adopt-scan"), &files(&[("/h/a", "x")])).unwrap_err();
        assert_eq!(e.code, Code::Conflict, "{e:?}");
        assert_eq!(e.code.exit(), Exit::Blocked);
    }

    #[test]
    fn a_non_utf8_path_is_blocked_before_any_write() {
        use std::os::unix::ffi::OsStrExt;
        let base = scratch("utf8");
        let r = fake(&base);
        let odd = base.join(std::ffi::OsStr::from_bytes(b"cache-\xff"));
        let e = scan(&r, &odd.join("adopt-scan"), &files(&[("/h/a", "x")])).unwrap_err();
        assert_eq!(e.code.exit(), Exit::Blocked, "{e:?}");
        assert!(!odd.exists());
        assert!(!base.join("log").exists(), "gitleaks did not run");
        let git = git_for(&base.join("home"));
        let e = dirty(&git, &odd, &["x"]).unwrap_err();
        assert_eq!(e.code.exit(), Exit::Blocked, "{e:?}");
        let e = inside_git_tree(&git, &odd, "x").unwrap_err();
        assert_eq!(e.code.exit(), Exit::Blocked, "{e:?}");
    }

    #[test]
    fn a_scan_removes_the_directories_of_dead_scans_only() {
        let base = scratch("sweep");
        let r = fake(&base);
        let parent = base.join("adopt-scan");
        let dir = |name: &str| {
            let d = parent.join(name);
            fs::create_dir_all(d.join("files")).unwrap();
            fs::write(d.join("files/x"), "left").unwrap();
            d
        };
        // Dead: a lock nobody holds, and an old directory with no lock (killed before it was
        // made, or an older binary's scan long past its timeout).
        fs::write(dir("11-1").join(SCAN_LOCK), "").unwrap();
        dir("12-2");
        // A fresh directory with no lock may be an older binary's scan that is still running.
        let fresh = format!("16-{}", now_ns().unwrap());
        dir(&fresh);
        // Live: a scan that holds its lock.
        let live = dir("13-3").join(SCAN_LOCK);
        fs::write(&live, "").unwrap();
        let held = fs::File::open(&live).unwrap();
        held.lock().unwrap();
        // Not scan directories: another name, a file and a link with a scan's name.
        dir("notes");
        fs::write(parent.join("14-4"), "").unwrap();
        std::os::unix::fs::symlink(base.join("bin"), parent.join("15-5")).unwrap();
        scan(&r, &parent, &files(&[("/h/a", "x")])).unwrap();
        let mut left: Vec<String> = fs::read_dir(&parent)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        let mut want = vec![
            "13-3".to_string(),
            "14-4".into(),
            "15-5".into(),
            fresh,
            "notes".into(),
        ];
        want.sort();
        assert_eq!(left, want);
        assert!(
            parent.join("13-3/files/x").is_file(),
            "a live scan keeps its copy"
        );
        assert!(
            base.join("bin/gitleaks").is_file(),
            "a link is not followed"
        );
        drop(held);
    }

    #[test]
    fn a_scan_waits_for_the_lock_another_scan_holds_briefly() {
        let base = scratch("wait");
        let r = fake(&base);
        let parent = base.join("adopt-scan");
        fs::create_dir_all(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let other = fs::File::open(&parent).unwrap();
        other.lock().unwrap();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(other);
        });
        assert!(
            scan(&r, &parent, &files(&[("/h/a", "x")]))
                .unwrap()
                .is_empty()
        );
        t.join().unwrap();
        assert_eq!(leftovers(&parent), 0);
    }

    #[test]
    fn a_scan_directory_that_is_a_link_is_a_conflict() {
        let base = scratch("link");
        let r = fake(&base);
        fs::create_dir_all(base.join("real")).unwrap();
        let parent = base.join("adopt-scan");
        std::os::unix::fs::symlink(base.join("real"), &parent).unwrap();
        let e = scan(&r, &parent, &files(&[("/h/a", "x")])).unwrap_err();
        assert_eq!(e.code, Code::Conflict, "{e:?}");
        assert_eq!(fs::read_dir(base.join("real")).unwrap().count(), 0);
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
            // A config beside the key in the same directory allowlists nothing either.
            (
                "/h/.config/near/.gitleaks.toml",
                "[allowlist]\nregexes = [\".*\"]\npaths = [\".*\"]\n",
            ),
            ("/h/.config/near/key", key),
        ]);
        let hits = scan(&r, &parent, &input).unwrap();
        assert_eq!(
            hits,
            [
                "/h/.config/near/key",
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
