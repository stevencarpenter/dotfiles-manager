//! `state check` (all builds) and the recording commands used to test the engine through the
//! binary (isolated builds only).

use super::{Ctx, bounded, header, usage};
use dfm_engine::{Fail, Store, verify_identity};
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
        (Some(_), None) => return writeln!(out, "bound; state is not initialized").map_err(w),
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
        binding.profile(),
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

pub fn dispatch_state(ctx: &Ctx, rest: &[String], out: &mut dyn Write) -> Result<(), Refusal> {
    match rest {
        [sub] if sub == "check" => check(ctx, out),
        _ => Err(usage("usage: state check")),
    }
}
