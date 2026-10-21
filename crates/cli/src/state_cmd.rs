//! `state check` (all builds) and the recording commands used to test the engine through the
//! binary (isolated builds only).

use super::{Ctx, bounded, header, usage};
use dfm_engine::{Dir, Fail, Finding, Store, Sys, recover, resolve, verify_identity};
use dfm_model::state::Phase;
use dfm_model::{Code, Refusal};
use std::io::Write;

#[cfg(feature = "test-isolation")]
mod harness;
#[cfg(feature = "test-isolation")]
pub use harness::{reconcile, record_op, record_trial};

/// Reads the failure point armed for this run. Release builds have no such input.
#[cfg(feature = "test-isolation")]
pub fn fail_from_env() -> Result<Fail, Refusal> {
    use dfm_engine::Action;
    let Some(spec) = std::env::var("DFM_FAILPOINT")
        .ok()
        .filter(|v| !v.is_empty())
    else {
        return Ok(Fail::none());
    };
    let (point, action) = spec
        .rsplit_once(':')
        .ok_or_else(|| usage("DFM_FAILPOINT is <point>:<error|abort|stop|wait-file>"))?;
    let action = match action {
        "error" => Action::Error,
        "abort" => Action::Abort,
        "stop" => Action::Stop,
        a => a
            .strip_prefix("wait-")
            .map(|f| Action::Wait(std::path::PathBuf::from(f)))
            .ok_or_else(|| usage("DFM_FAILPOINT action is error, abort, stop or wait-<file>"))?,
    };
    Ok(Fail::at(point, action))
}

#[cfg(not(feature = "test-isolation"))]
pub fn fail_from_env() -> Result<Fail, Refusal> {
    Ok(Fail::none())
}

/// Read-only: loads the binding and every state document and checks that each one belongs to the
/// bound identity. It lists every problem and repairs nothing. A running writer makes the open
/// intents and temporary files it leaves look like damage, so check refuses (`E_LOCKED`, no
/// findings) while one holds the lock. It holds the lock shared until it returns, so a writer
/// that starts during the read gets `E_LOCKED` instead of being reported as damage. Without a
/// lock file it holds no lock, so a lock file that appears during the read is `E_LOCKED` too.
pub fn check(ctx: &Ctx, out: &mut dyn Write) -> Result<(), Refusal> {
    let w =
        |e: std::io::Error| Refusal::new(Code::IoFailed, format!("output failed: {}", e.kind()));
    header(out, ctx).map_err(w)?;
    // Topology and directory-level refusals are findings here (exit 1), as for a diagnostic
    // command, not an abort: the report still says what was found.
    let finding = |r: Refusal, out: &mut dyn Write| -> Result<(), Refusal> {
        writeln!(out, "{} {}", r.code.as_str(), bounded(&r.detail)).map_err(w)?;
        Err(Refusal::new(Code::Diagnosed, "1 finding"))
    };
    let shared = match ctx
        .sys()
        .and_then(|sys| Store::probe_writer(&sys, &ctx.layout()))
    {
        Ok(guard) => guard,
        Err(r) if r.code == Code::Locked => {
            writeln!(
                out,
                "a writer is running; run state check again when it ends"
            )
            .map_err(w)?;
            return Err(r);
        }
        Err(r) => return finding(r, out),
    };
    fail_from_env()?.hit("check-after-probe")?;
    // Called after the read and before anything is reported, so a lock file replaced during the
    // read, or one that a first `bind` created during it, leaves no findings.
    let recheck = || -> Result<(), Refusal> {
        let sys = ctx.sys()?;
        match &shared {
            Some(guard) => Store::recheck_probe(&sys, &ctx.layout(), guard),
            None => match Store::probe_writer(&sys, &ctx.layout())? {
                None => Ok(()),
                Some(_) => Err(Refusal::new(
                    Code::Locked,
                    "a writer started during the read; run state check again",
                )),
            },
        }
    };
    let binding = match super::read_existing_binding(ctx) {
        Ok(b) => b,
        Err(r) => {
            recheck()?;
            return finding(r, out);
        }
    };
    let store = match ctx
        .sys()
        .and_then(|sys| Store::open_read(sys, &ctx.layout(), Fail::none()))
    {
        Ok(s) => s,
        Err(r) => {
            recheck()?;
            return finding(r, out);
        }
    };
    if !matches!((&binding, &store), (Some(_), Some(_))) {
        recheck()?;
    }
    let (binding, store) = match (binding, store) {
        (Some(b), Some(s)) => (b, s),
        (None, None) => return writeln!(out, "unbound; no state").map_err(w),
        (Some(_), None) => {
            writeln!(
                out,
                "E_STATE_INVALID the binding exists but its state directory is missing (state loss)"
            )
            .map_err(w)?;
            return Err(Refusal::new(Code::Diagnosed, "1 finding"));
        }
        (None, Some(_)) => {
            writeln!(out, "E_STATE_INVALID state exists but there is no binding").map_err(w)?;
            return Err(Refusal::new(Code::Diagnosed, "1 finding"));
        }
    };
    let l = store.load();
    recheck()?;
    let mut findings = verify_identity(&l, &binding);
    // An interrupted write leaves evidence on disk, so a stray entry fails the check.
    findings.extend(l.strays.iter().map(|s| Finding {
        code: Code::StateInvalid,
        file: s.clone(),
        detail: "stray entry from an interrupted write or an unexpected file. Fix: read it, then remove it by hand after you confirm that no apply is running; `state recover` does not delete it".into(),
    }));
    let count = |n: usize, bad: usize| format!("{n} ({bad} unreadable)");
    let (r_bad, t_bad) = (
        l.receipts.iter().filter(|r| r.1.is_err()).count(),
        l.trials.iter().filter(|r| r.1.is_err()).count(),
    );
    writeln!(
        out,
        "binding: {} as {}",
        dfm_model::display::visible(binding.profile()),
        binding.machine_id()
    )
    .map_err(w)?;
    if let Ok(led) = &l.ledger {
        let outputs: usize = led.resources.iter().map(|r| r.owned_outputs.len()).sum();
        writeln!(
            out,
            "ledger: {} resources, {outputs} owned outputs",
            led.resources.len()
        )
        .map_err(w)?;
    }
    writeln!(out, "receipts: {}", count(l.receipts.len(), r_bad)).map_err(w)?;
    writeln!(out, "trials: {}", count(l.trials.len(), t_bad)).map_err(w)?;
    if let Ok(j) = &l.journal {
        writeln!(out, "journal: {} entries", j.len()).map_err(w)?;
    }
    for f in &findings {
        writeln!(
            out,
            "{} {}: {}",
            f.code.as_str(),
            bounded(&f.file),
            bounded(&f.detail)
        )
        .map_err(w)?;
    }
    if findings.is_empty() {
        writeln!(out, "state ok").map_err(w)
    } else {
        Err(Refusal::new(
            Code::Diagnosed,
            format!("{} findings", findings.len()),
        ))
    }
}

/// A store opened for writing under the writer lock, with the opened HOME.
struct Locked {
    store: Store,
    home: Dir,
    sys: Sys,
    home_path: std::path::PathBuf,
}

fn open_locked(ctx: &Ctx) -> Result<Locked, Refusal> {
    let binding = super::read_existing_binding(ctx)?.ok_or_else(|| {
        Refusal::new(
            Code::BindingInvalid,
            "this machine is not bound; run bind first",
        )
    })?;
    let (layout, sys) = (ctx.layout(), ctx.sys()?);
    let store = Store::open_write_with(sys, &layout, fail_from_env()?, false)?;
    if !store.is_initialized() {
        return Err(store.stamp(super::state_lost_refusal()));
    }
    store
        .require_identity(&binding)
        .map_err(|r| store.stamp(r))?;
    let home = Dir::open_home(&sys, &layout.home).map_err(|r| store.stamp(r))?;
    Ok(Locked {
        store,
        home,
        sys,
        home_path: layout.home,
    })
}

fn out_err(e: std::io::Error) -> Refusal {
    Refusal::new(Code::IoFailed, format!("output failed: {}", e.kind()))
}

/// `state resolve <journal-id> --as done|failed`: settles an `unknown` outcome that no probe
/// can decide. It takes the writer lock and appends one journal entry with the reason
/// "resolved by operator". `done` needs the live path to match what the operation expected,
/// and then records ownership under the same shape rules as apply. No receipt is written: the
/// journal entry is the record.
pub fn resolve_cmd(ctx: &Ctx, id: &str, as_done: bool, out: &mut dyn Write) -> Result<(), Refusal> {
    let l = open_locked(ctx)?;
    resolve(&l.store, (&l.sys, &l.home, &l.home_path), id, as_done)
        .map_err(|r| l.store.stamp(r))?;
    header(out, ctx).map_err(out_err)?;
    writeln!(
        out,
        "resolved {} as {}",
        bounded(id),
        if as_done { "done" } else { "failed" }
    )
    .map_err(out_err)
}

/// `state recover`: settles what the machine shows clearly, with no plan and no allowance
/// flags. It takes the writer lock and writes journal entries (and ledger entries for a `done`
/// that apply would own). It writes no receipt. Exit 0 when nothing stays open, else 3 with
/// `E_OP_INTERRUPTED` naming each journal id.
pub fn recover_cmd(ctx: &Ctx, out: &mut dyn Write) -> Result<(), Refusal> {
    let l = open_locked(ctx)?;
    let s = recover(&l.store, (&l.sys, &l.home, &l.home_path)).map_err(|r| l.store.stamp(r))?;
    header(out, ctx).map_err(out_err)?;
    if s.repaired > 0 {
        writeln!(
            out,
            "repaired a torn final journal line ({} bytes removed)",
            s.repaired
        )
        .map_err(out_err)?;
    }
    for warning in &s.warnings {
        writeln!(out, "warning: {}", bounded(warning)).map_err(out_err)?;
    }
    for (id, phase) in &s.closed {
        let what = if *phase == Phase::Done {
            "done"
        } else {
            "failed"
        };
        writeln!(out, "settled {} as {what}", bounded(id)).map_err(out_err)?;
    }
    for o in &s.open {
        writeln!(
            out,
            "open {} ({} of {}) {}",
            bounded(&o.id),
            o.op,
            bounded(&o.resource),
            o.paths
                .iter()
                .map(|p| bounded(p))
                .collect::<Vec<_>>()
                .join(", ")
        )
        .map_err(out_err)?;
    }
    if s.closed.is_empty() && s.open.is_empty() && s.repaired == 0 && s.warnings.is_empty() {
        return writeln!(out, "nothing to recover").map_err(out_err);
    }
    // An open entry is a result to report, not a failure of this command: exit 3 even when
    // other entries were settled.
    match s.open.first() {
        None => Ok(()),
        Some(first) => Err(Refusal::new(
            Code::OpInterrupted,
            format!(
                "{} operation(s) stay open: {}. Nothing the machine shows clearly is left unsettled. Check the path, then run `dfm state resolve '{}' --as done|failed`{}",
                s.open.len(),
                s.open
                    .iter()
                    .map(|o| bounded(&o.id))
                    .collect::<Vec<_>>()
                    .join(", "),
                bounded(&first.id),
                if s.open.iter().any(|o| o.paths.len() > 1) {
                    " (an entry with more than one path cannot be resolved as done)"
                } else {
                    ""
                }
            ),
        )),
    }
}

pub fn dispatch_state(ctx: &Ctx, rest: &[String], out: &mut dyn Write) -> Result<(), Refusal> {
    const USAGE: &str =
        "usage: state check | state recover | state resolve <journal-id> --as done|failed";
    match rest {
        [sub] if sub == "check" => check(ctx, out),
        [sub] if sub == "recover" => recover_cmd(ctx, out),
        [sub, id, flag, kind] if sub == "resolve" && flag == "--as" => match kind.as_str() {
            "done" | "failed" => resolve_cmd(ctx, id, kind == "done", out),
            _ => Err(usage("state resolve: --as is done or failed")),
        },
        _ => Err(usage(USAGE)),
    }
}
