//! `apply`: publishes and removes the files of an approved plan (docs/apply.md).
//!
//! The command plans again from the current machine, requires the digest the reviewer approved
//! (`--yes-plan`), takes the writer lock, plans a second time under the lock and compares again,
//! and only then hands the plan to the engine. Without an approval it prints the plan and
//! changes nothing.

use crate::plan_cmd::{make_plan, render};
use crate::{Ctx, bounded, read_existing_binding, state_cmd, usage};
use dfm_engine::{Dir, NoGenerator, Run, Store, apply, now_unix, preflight};
use dfm_model::display::visible;
use dfm_model::state::{
    Approval, BinaryId, ProfileRef, Receipt, RunResult, Stamp, Status, count_statuses, rfc3339,
};
use dfm_model::{Code, Exit, Refusal};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

pub struct Args {
    pub root: String,
    pub yes_plan: Option<String>,
    pub allow: BTreeSet<String>,
    pub preview: bool,
}

/// `apply --root <dir> [--yes-plan <digest>] [--allow-disruptive <operation-id>]... [--preview]`.
pub fn parse_args(args: &[String]) -> Result<Args, Refusal> {
    let (mut root, mut yes, mut preview) = (None, None, false);
    let mut allow = BTreeSet::new();
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
            "--allow-disruptive" => {
                let v = value(None)?;
                allow.insert(v);
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
        Status::Updated => "applied",
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

pub fn run(ctx: &Ctx, args: &Args, out: &mut dyn Write) -> Result<(), Refusal> {
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
    let plan = make_plan(ctx, root)?;
    let digest = plan.digest()?;
    if let Some(i) = &ctx.isolated {
        writeln!(out, "isolated: true {i}").map_err(w)?;
    }
    render(&plan, &digest, out).map_err(w)?;
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
    let (layout, sys) = (ctx.layout(), ctx.sys()?);
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
    let locked = make_plan(ctx, root).map_err(|r| stamped(&store, r))?;
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
        generator: &NoGenerator,
        allow: &args.allow,
        fail: &fail,
    };
    preflight(&run, &locked).map_err(|r| stamped(&store, r))?;

    let started = now_unix().map_err(|r| stamped(&store, r))?;
    let report = apply(&run, &locked).map_err(|r| stamped(&store, r))?;
    let finished = now_unix().map_err(Refusal::after_mutation)?;

    let counts = count_statuses(&report.operations);
    let n = |s: Status| counts.get(&s).copied().unwrap_or(0);
    let bad = n(Status::Failed) + n(Status::Blocked) + n(Status::Unknown) > 0;
    let mutated = store.mutated();
    let exit = match (bad, mutated) {
        (false, _) => Exit::Ok,
        (true, true) => Exit::Failed,
        (true, false) => Exit::Blocked,
    };

    // The receipt is written whenever anything changed, also after a failure. A run that
    // changed nothing (a safe rerun, or a stop before the first effect) writes none.
    let mut receipt_name = None;
    if mutated {
        let stamp = Stamp::of(&binding);
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
            // A path source has no commit. The plan digest covers the source tree digest.
            sources: Vec::new(),
            operations: report.operations.clone(),
            result: RunResult {
                outcome: exit.name().into(),
                exit_code: exit.code(),
                counts: counts.clone(),
            },
        };
        receipt_name = Some(
            store
                .write_receipt(&receipt)
                .map_err(Refusal::after_mutation)?,
        );
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
        "counts: applied={} current={} failed={} blocked={} not_started={} not_in_scope={}",
        n(Status::Updated),
        n(Status::Current),
        n(Status::Failed),
        n(Status::Blocked),
        n(Status::NotStarted),
        n(Status::NotInScope)
    )
    .map_err(w)?;
    if n(Status::Updated) > 0 {
        writeln!(
            out,
            "reload: not attempted. Publishing a file does not reload the program that reads it"
        )
        .map_err(w)?;
    }
    if let Some(r) = &receipt_name {
        writeln!(out, "receipt: {}", visible(r)).map_err(w)?;
    }
    writeln!(out, "result: {}", exit.name()).map_err(w)?;
    match (exit, report.stopped) {
        (Exit::Ok, _) => Ok(()),
        // Nothing changed: the first failure keeps its own code, which exits 3.
        (Exit::Blocked, Some(r)) => Err(r),
        (_, stopped) => Err(Refusal::new(
            Code::ApplyFailed,
            bounded(&format!(
                "{} failed, {} blocked, {} not started after some files changed. First failure: {}. The ledger lists exactly what was published; run plan to see what remains",
                n(Status::Failed),
                n(Status::Blocked),
                n(Status::NotStarted),
                stopped.map_or_else(|| "unknown".into(), |r| r.to_string())
            )),
        )),
    }
}
