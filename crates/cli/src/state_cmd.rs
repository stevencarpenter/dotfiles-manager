//! `state check` (all builds) and the recording commands used to test the engine through the
//! binary (isolated builds only).

use super::{Ctx, bounded, header, usage};
use dfm_engine::{Dir, Fail, Store, resolve, verify_identity};
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
        .ok_or_else(|| usage("DFM_FAILPOINT is <point>:<error|abort|wait-file>"))?;
    let action = match action {
        "error" => Action::Error,
        "abort" => Action::Abort,
        a => a
            .strip_prefix("wait-")
            .map(|f| Action::Wait(std::path::PathBuf::from(f)))
            .ok_or_else(|| usage("DFM_FAILPOINT action is error, abort or wait-<file>"))?,
    };
    Ok(Fail::at(point, action))
}

#[cfg(not(feature = "test-isolation"))]
pub fn fail_from_env() -> Result<Fail, Refusal> {
    Ok(Fail::none())
}

/// Read-only: loads the binding and every state document and checks that each one belongs to the
/// bound identity. It lists every problem, repairs nothing and takes no lock.
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
    let binding = match super::read_existing_binding(ctx) {
        Ok(b) => b,
        Err(r) => return finding(r, out),
    };
    let store = match ctx
        .sys()
        .and_then(|sys| Store::open_read(sys, &ctx.layout(), Fail::none()))
    {
        Ok(s) => s,
        Err(r) => return finding(r, out),
    };
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
    let findings = verify_identity(&l, &binding);
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
    for s in &l.strays {
        writeln!(out, "note: {}", bounded(s)).map_err(w)?;
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

/// `state resolve <journal-id> --as done|failed`: settles an `unknown` outcome that no probe
/// can decide. It takes the writer lock and appends one journal entry with the reason
/// "resolved by operator". `done` needs the live path to match what the operation expected,
/// and then records ownership as the operation would have. No receipt is written: the journal
/// entry is the record.
pub fn resolve_cmd(ctx: &Ctx, id: &str, as_done: bool, out: &mut dyn Write) -> Result<(), Refusal> {
    let w =
        |e: std::io::Error| Refusal::new(Code::IoFailed, format!("output failed: {}", e.kind()));
    let binding = super::read_existing_binding(ctx)?.ok_or_else(|| {
        Refusal::new(
            Code::BindingInvalid,
            "this machine is not bound; run bind first",
        )
    })?;
    let (layout, sys) = (ctx.layout(), ctx.sys()?);
    let store = Store::open_write_with(sys, &layout, fail_from_env()?, false)?;
    let stamped = |store: &Store, r: Refusal| {
        if store.mutated() {
            r.after_mutation()
        } else {
            r
        }
    };
    if !store.is_initialized() {
        return Err(stamped(&store, super::state_lost_refusal()));
    }
    store
        .require_identity(&binding)
        .map_err(|r| stamped(&store, r))?;
    let home = Dir::open_home(&sys, &layout.home).map_err(|r| stamped(&store, r))?;
    resolve(&store, (&sys, &home, &layout.home), id, as_done).map_err(|r| stamped(&store, r))?;
    header(out, ctx).map_err(w)?;
    writeln!(
        out,
        "resolved {} as {}",
        bounded(id),
        if as_done { "done" } else { "failed" }
    )
    .map_err(w)
}

pub fn dispatch_state(ctx: &Ctx, rest: &[String], out: &mut dyn Write) -> Result<(), Refusal> {
    const USAGE: &str = "usage: state check | state resolve <journal-id> --as done|failed";
    match rest {
        [sub] if sub == "check" => check(ctx, out),
        [sub, id, flag, kind] if sub == "resolve" && flag == "--as" => match kind.as_str() {
            "done" | "failed" => resolve_cmd(ctx, id, kind == "done", out),
            _ => Err(usage("state resolve: --as is done or failed")),
        },
        _ => Err(usage(USAGE)),
    }
}
