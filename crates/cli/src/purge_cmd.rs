//! `purge`: end a trial by removing what it added (docs/purge.md). The command uninstalls the
//! package the trial installed and the Homebrew dependencies that install added, removes every
//! recorded configuration and data path that was absent before the trial, then removes the
//! trial record. Anything that existed before the trial is kept. The run needs `--yes-plan`
//! with the digest of the preview, as `trial` and `adopt` do.

use crate::adapters::package::{self, HOMEBREW};
use crate::adopt_cmd::overlaps;
use crate::apply_cmd::{is_digest, label, stale};
use crate::{Ctx, paint, read_existing_binding, state_cmd, usage};
use dfm_engine::fsys::{Dir, Peek, Sys};
use dfm_engine::{Runner, Store, now_ns, signal};
use dfm_model::digest::sha256_hex;
use dfm_model::display::{bounded_visible, visible};
use dfm_model::fold::fold;
use dfm_model::plan::{Binary, PackageState, file_fingerprint};
use dfm_model::resolve::{Table, Value};
use dfm_model::state::{
    Approval, BaselinePath, BinaryId, Class, JournalEntry, MAX_BYTES, OperationRecord, Phase,
    ProfileRef, Receipt, Recovery, RecoveryOutput, RunResult, SCHEMA, Stamp, Status, TrialRecord,
    count_statuses, rfc3339,
};
use dfm_model::{Code, Exit, Refusal};
use serde_json::json;
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// The most entries a removed directory may hold. A larger tree is kept: its preview could not
/// show what the approval covers.
const MAX_ENTRIES: usize = 100_000;

#[derive(Debug)]
pub struct Args {
    /// The trial id, or the tool name of a trial with the default id `trial-<tool>`.
    pub trial: String,
    pub preview: bool,
    pub yes_plan: Option<String>,
}

/// `purge <trial-id> [--preview] [-y|--yes-plan <digest>]`.
pub fn parse_args(args: &[String]) -> Result<Args, Refusal> {
    let (mut trial, mut yes, mut preview) = (None, None, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--yes-plan" | "-y" => {
                let v = it
                    .next()
                    .ok_or_else(|| usage(format!("{a} needs a value")))?;
                if yes.replace(v.clone()).is_some() {
                    return Err(usage(format!("{a} given twice")));
                }
            }
            "--preview" if !preview => preview = true,
            "--preview" => return Err(usage("--preview given twice")),
            other if other.starts_with('-') => {
                return Err(usage(format!("unknown argument {other}")));
            }
            _ if trial.is_some() => return Err(usage("purge takes one trial id")),
            _ => trial = Some(a.clone()),
        }
    }
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
    unknown: Vec<String>,
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
            Now::Other(w) => json!({ "other": w }),
        };
        let doc = json!({
            "trial_id": self.trial_id,
            "tool": self.tool,
            "package": self.package.as_ref().map(|(a, n, s)| json!({"adapter": a, "name": n, "step": step_json(s)})),
            "deps": self.deps.iter().map(|(n, s)| json!({"name": n, "step": step_json(s)})).collect::<Vec<_>>(),
            "paths": self.paths.iter().map(|p| json!({"path": p.path, "now": now(&p.now), "step": step_json(&p.step)})).collect::<Vec<_>>(),
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
        match &self.package {
            None => writeln!(out, "package: none recorded; the trial installed nothing")?,
            Some((a, n, s)) => line(out, &format!("uninstall {a} {}", visible(n)), s)?,
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
            let what = match &p.now {
                Now::Absent => String::new(),
                Now::File(h) if h.starts_with("size:") => format!(" file ({h})"),
                Now::File(h) => format!(" file sha256:{h}"),
                Now::Dir(n, b, _) => format!(" directory ({n} entries, {b} bytes)"),
                Now::Other(w) => format!(" {w}"),
            };
            line(out, &format!("remove {}{what}", visible(&p.path)), &p.step)?;
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

/// Whether two recorded names are one package: brew treats `atuin` and `homebrew/core/atuin`
/// as the same formula. Other adapters compare the names as written.
fn same_package(adapter: &str, a: &str, b: &str) -> bool {
    if adapter == HOMEBREW {
        package::brewfile_names(a).0 == package::brewfile_names(b).0
    } else {
        a == b
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
    if let Some(why) = unremovable(&fs::symlink_metadata(dir)?, uid) {
        return Ok(Err(why));
    }
    let mut lines = Vec::new();
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
            lines.push(format!("{rel}\t{kind}\t{}", m.len()));
            if lines.len() > MAX_ENTRIES {
                return Ok(Err(format!("more than {MAX_ENTRIES} entries")));
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

/// One recorded path now, and what purge does with it.
/// `others` are the paths the other active trials recorded.
fn path_item(
    b: &BaselinePath,
    owned: &[String],
    others: &[(String, String)],
    sys: &Sys,
    home: &Path,
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
    } else {
        Step::Do
    };
    item(now, step)
}

/// Removes one path below HOME without following a link: every parent is opened as a real
/// directory owned by this user, then the entry is removed (a directory with its tree).
fn remove_path(
    store: &Store,
    sys: &Sys,
    home: &Path,
    path: &str,
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
    match parent.peek(sys, leaf)? {
        Peek::Missing => Ok(()),
        Peek::Dir { .. } => parent.remove_tree(sys, leaf),
        Peek::File { .. } => parent.remove_entry(leaf, fail),
        _ => Err(Refusal::new(
            Code::Conflict,
            format!("{path} became a link or a special file; it is not removed"),
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
                o.baseline
                    .paths
                    .iter()
                    .chain(&o.baseline.data)
                    .map(|b| (o.trial_id.clone(), b.path.clone()))
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
        let paths = t
            .baseline
            .paths
            .iter()
            .chain(&t.baseline.data)
            .map(|b| path_item(b, &owned, &other_paths, &sys, &layout.home))
            .collect();
        let d = Decision {
            trial_id: t.trial_id.clone(),
            tool: t.tool.clone(),
            package,
            deps,
            paths,
            unknown: t.unknown.clone(),
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
                remove_path(&store, &sys, &layout.home, &p.path, &fail)
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
