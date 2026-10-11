//! `apply`: publishes and removes the files of an approved plan (docs/apply.md).
//!
//! The command plans again from the current machine, requires the digest the reviewer approved
//! (`--yes-plan`), takes the writer lock, plans a second time under the lock and compares again,
//! and only then hands the plan to the engine. Without an approval it prints the plan and
//! changes nothing.

use crate::adapters::Generators;
use crate::plan_cmd::{plan_for, render};
use crate::{Ctx, load_resolved, read_existing_binding, state_cmd, usage};
use dfm_engine::{
    Approval as Allowance, Dir, Run, Store, apply, check_allowances, check_journal_capacity, gates,
    now_unix,
};
use dfm_model::display::visible;
use dfm_model::state::{
    Approval, BinaryId, DisruptiveApproval, ProfileRef, Receipt, RunResult, SourceId, Stamp,
    Status, count_statuses, rfc3339,
};
use dfm_model::{Code, Exit, Refusal};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

pub struct Args {
    pub root: String,
    pub yes_plan: Option<String>,
    pub allow: BTreeSet<String>,
    pub allow_adopt: BTreeSet<String>,
    pub preview: bool,
}

/// `apply --root <dir> [--yes-plan <digest>] [--allow-disruptive <operation-id>]...
/// [--allow-adopt <operation-id>]... [--preview]`.
pub fn parse_args(args: &[String]) -> Result<Args, Refusal> {
    let (mut root, mut yes, mut preview) = (None, None, false);
    let (mut allow, mut allow_adopt) = (BTreeSet::new(), BTreeSet::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |slot: Option<&mut Option<String>>| -> Result<String, Refusal> {
            let v = it
                .next()
                .ok_or_else(|| usage(format!("{a} needs a value")))?;
            if let Some(slot) = slot
                && slot.replace(v.clone()).is_some()
            {
                return Err(usage(format!("{a} given twice")));
            }
            Ok(v.clone())
        };
        match a.as_str() {
            "--root" => {
                value(Some(&mut root))?;
            }
            "--yes-plan" => {
                value(Some(&mut yes))?;
            }
            flag if Allowance::from_flag(flag).is_some() => {
                let v = value(None)?;
                match Allowance::from_flag(flag) {
                    Some(Allowance::Adopt) => allow_adopt.insert(v),
                    _ => allow.insert(v),
                };
            }
            "--preview" if !preview => preview = true,
            "--preview" => return Err(usage("--preview given twice")),
            other => return Err(usage(format!("unknown argument {other}"))),
        }
    }
    if preview && yes.is_some() {
        return Err(usage("--preview and --yes-plan cannot be used together"));
    }
    if let Some(d) = &yes
        && !(d.len() == 64
            && d.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
    {
        return Err(usage(
            "--yes-plan takes the 64 hex characters of a plan digest",
        ));
    }
    Ok(Args {
        root: root.ok_or_else(|| usage("--root is required"))?,
        yes_plan: yes,
        allow,
        allow_adopt,
        preview,
    })
}

fn stale(approved: &str, now: &str) -> Refusal {
    Refusal::new(
        Code::PlanStale,
        format!(
            "the approved plan digest {approved} is not the digest of the plan for this machine now ({now}). Fix: read the new plan with --preview and approve its digest"
        ),
    )
}

fn label(s: Status) -> &'static str {
    match s {
        Status::Updated => "updated",
        Status::Current => "current",
        Status::Failed => "failed",
        Status::Blocked => "blocked",
        Status::NotStarted => "not_started",
        Status::NotInScope => "not_in_scope",
        Status::Unknown => "unknown",
        Status::Held => "held",
        Status::Pending => "pending",
        Status::BlockedPending => "blocked_pending",
    }
}

/// What was printed so far, so a refusal prints its `result:` line exactly once.
#[derive(Default)]
struct Progress {
    plan_printed: bool,
    result_printed: bool,
}

pub fn run(ctx: &Ctx, args: &Args, out: &mut dyn Write) -> Result<(), Refusal> {
    let mut progress = Progress::default();
    let r = run_inner(ctx, args, out, &mut progress);
    if let Err(e) = &r
        && progress.plan_printed
        && !progress.result_printed
    {
        // The printed result is the exit the process returns, from the same refusal.
        let _ = writeln!(out, "result: {}", e.exit().name());
    }
    r
}

fn run_inner(
    ctx: &Ctx,
    args: &Args,
    out: &mut dyn Write,
    progress: &mut Progress,
) -> Result<(), Refusal> {
    let w =
        |e: std::io::Error| Refusal::new(Code::IoFailed, format!("output failed: {}", e.kind()));
    let binding = read_existing_binding(ctx)?.ok_or_else(|| {
        Refusal::new(
            Code::BindingInvalid,
            "this machine is not bound; run bind first",
        )
    })?;
    let root = Path::new(&args.root);
    // Read-only look first. A refusal here changes nothing, not even the lock file.
    let first = load_resolved(ctx, root)?;
    let plan = plan_for(ctx, &first)?;
    let digest = plan.digest()?;
    if let Some(i) = &ctx.isolated {
        writeln!(out, "isolated: true {i}").map_err(w)?;
    }
    render(&plan, &digest, out).map_err(w)?;
    progress.plan_printed = true;
    // A digest that no longer matches is the first thing to report: the reviewer approved a
    // different plan. A blocked plan cannot be approved at all.
    if let Some(approved) = &args.yes_plan
        && *approved != digest
    {
        return Err(stale(approved, &digest));
    }
    if let Some(b) = plan.blockers.first() {
        return Err(b.refusal());
    }
    // The gates that need no write run before the lock and in a preview, so a plan that apply
    // would refuse is refused here with the same code.
    let (layout, sys) = (ctx.layout(), ctx.sys()?);
    let g = gates(&plan, &Generators::new(&first.resolved));
    if let Some(r) = g.refusal {
        return Err(r);
    }
    // An id that names no operation of its kind is a usage error and wins over a missing
    // approval. A missing approval is refused with every missing flag printed to paste.
    if let Err(all) = check_allowances(&g, &args.allow, &args.allow_adopt) {
        for e in all.iter().filter(|e| e.code != Code::Usage) {
            writeln!(out, "{}", visible(&e.detail)).map_err(w)?;
        }
        return Err(all
            .into_iter()
            .next()
            .unwrap_or_else(|| usage("approval check failed")));
    }
    // The journal size is read without the lock. The lock is taken only for a real run.
    let readable = Store::open_read(sys, &layout, state_cmd::fail_from_env()?)?
        .ok_or_else(crate::state_lost_refusal)?;
    check_journal_capacity(readable.journal_len()?, &plan)?;
    drop(readable);
    if args.preview {
        return Ok(());
    }
    let Some(approved) = &args.yes_plan else {
        return Err(Refusal::new(
            Code::ApprovalRequired,
            format!(
                "nothing was changed. Fix: read the plan above, then run again with --yes-plan {digest}"
            ),
        ));
    };

    // Take the lock, then decide again: another command may have changed the machine or the
    // sources since the first look.
    let fail = state_cmd::fail_from_env()?;
    let store = Store::open_write_with(sys, &layout, fail.clone(), false)?;
    let stamped = |store: &Store, r: Refusal| {
        if store.mutated() {
            r.after_mutation()
        } else {
            r
        }
    };
    if !store.is_initialized() {
        return Err(stamped(&store, crate::state_lost_refusal()));
    }
    store
        .require_identity(&binding)
        .map_err(|r| stamped(&store, r))?;
    let second = load_resolved(ctx, root).map_err(|r| stamped(&store, r))?;
    let locked = plan_for(ctx, &second).map_err(|r| stamped(&store, r))?;
    let locked_digest = locked.digest().map_err(|r| stamped(&store, r))?;
    if locked_digest != digest {
        return Err(stamped(&store, stale(approved, &locked_digest)));
    }
    let home = Dir::open_home(&sys, &layout.home).map_err(|r| stamped(&store, r))?;
    let run = Run {
        store: &store,
        sys,
        home: &home,
        home_path: &layout.home,
        generator: &Generators::new(&second.resolved),
        allow: &args.allow,
        allow_adopt: &args.allow_adopt,
        fail: &fail,
    };
    let started = now_unix().map_err(|r| stamped(&store, r))?;
    let report = apply(&run, &locked).map_err(|r| stamped(&store, r))?;
    let finished = now_unix().unwrap_or(started);

    let counts = count_statuses(&report.operations);
    let n = |s: Status| counts.get(&s).copied().unwrap_or(0);
    let mutated = store.mutated();
    // One exit, derived from the first problem, is both printed and returned.
    let stopped = report
        .stopped
        .map(|r| if mutated { r.after_mutation() } else { r });
    let mut exit = stopped.as_ref().map_or(Exit::Ok, Refusal::exit);

    // The receipt is written whenever anything changed, also after a failure. A run that
    // changed nothing (a safe rerun, or a stop before the first effect) writes none.
    let mut receipt_name = None;
    let mut receipt_error = None;
    if mutated {
        let stamp = Stamp::of(&binding);
        let approvals = locked
            .operations
            .iter()
            .zip(&report.operations)
            .filter_map(|(op, rec)| {
                let kind = Allowance::of(op.action)?;
                let approved = match kind {
                    Allowance::Disruptive => &args.allow,
                    Allowance::Adopt => &args.allow_adopt,
                };
                // The attempt id replaces the planner id once the operation reached its intent.
                (approved.contains(&op.id) && rec.id != op.journal_id).then(|| DisruptiveApproval {
                    operation_id: rec.id.clone(),
                    how: kind.how().into(),
                    at: rec.started_at.clone().unwrap_or_else(|| rfc3339(started)),
                })
            })
            .collect();
        let sources = locked
            .sources
            .iter()
            .filter(|s| s.kind == "path")
            .map(|s| SourceId {
                id: format!(
                    "path-{}",
                    s.tree
                        .trim_start_matches("sha256:")
                        .get(..12)
                        .unwrap_or("0")
                ),
                commit: None,
                tree: s.tree.clone(),
            })
            .collect();
        let receipt = Receipt {
            schema: 1,
            op_id: format!("apply-{}-{}", &digest[..16], std::process::id()),
            command: "apply".into(),
            started_at: rfc3339(started),
            finished_at: rfc3339(finished),
            binary: BinaryId {
                version: locked.binary.version.clone(),
                commit: option_env!("DFM_BUILD_COMMIT").unwrap_or("unknown").into(),
                sha256: locked.binary.sha256.clone(),
            },
            profile: ProfileRef {
                id: stamp.profile,
                binding_digest: stamp.binding_digest,
            },
            machine_id: stamp.machine_id,
            plan_digest: Some(digest.clone()),
            approval: Some(Approval {
                kind: "yes_plan".into(),
                plan_digest: digest.clone(),
                at: rfc3339(started),
            }),
            sources,
            approvals,
            operations: report.operations.clone(),
            result: RunResult {
                outcome: exit.name().into(),
                exit_code: exit.code(),
                counts: counts.clone(),
            },
        };
        match store.write_receipt(&receipt) {
            Ok(name) => receipt_name = Some(name),
            Err(e) => {
                // The journal and the ledger stay the record. The run is a failure.
                exit = Exit::Failed;
                receipt_error = Some(e.after_mutation());
            }
        }
    }

    for (op, rec) in locked.operations.iter().zip(&report.operations) {
        let reload = rec
            .reload
            .as_deref()
            .map_or(String::new(), |r| format!(" reload={r}"));
        let error = rec
            .error
            .as_deref()
            .map_or(String::new(), |e| format!(": {}", visible(e)));
        writeln!(
            out,
            "  {} {}{reload}{error}",
            label(rec.status),
            visible(&op.id)
        )
        .map_err(w)?;
    }
    writeln!(
        out,
        "counts: updated={} current={} failed={} blocked={} unknown={} not_started={} not_in_scope={}",
        n(Status::Updated),
        n(Status::Current),
        n(Status::Failed),
        n(Status::Blocked),
        n(Status::Unknown),
        n(Status::NotStarted),
        n(Status::NotInScope)
    )
    .map_err(w)?;
    // An adoption publishes nothing, so only a published output earns this note.
    if report.operations.iter().any(|r| r.reload.is_some()) {
        writeln!(
            out,
            "reload: not attempted. Publishing a file does not reload the program that reads it"
        )
        .map_err(w)?;
    }
    if let Some(r) = &receipt_name {
        writeln!(out, "receipt: {}", visible(r)).map_err(w)?;
    }
    if let Some(e) = &receipt_error {
        writeln!(out, "receipt: not written ({})", visible(&e.to_string())).map_err(w)?;
    }
    writeln!(out, "result: {}", exit.name()).map_err(w)?;
    progress.result_printed = true;
    match (receipt_error, stopped) {
        (Some(e), _) => Err(e),
        (None, Some(r)) => Err(r),
        (None, None) => Ok(()),
    }
}
