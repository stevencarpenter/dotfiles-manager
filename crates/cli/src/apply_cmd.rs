//! `apply`: publishes and removes the files of an approved plan (docs/apply.md). `secrets` is
//! the same command over the secrets scope (docs/secrets.md): it renders secret outputs and
//! nothing else, and its gates add the vault session check. `packages` (docs/packages.md) and
//! `services` (docs/lifecycle.md) are the same command over their scopes, with the providers
//! probed before the plan.
//!
//! The command plans again from the current machine, requires the digest the reviewer approved
//! (`--yes-plan`), takes the writer lock, plans a second time under the lock and compares again,
//! and only then hands the plan to the engine. Without an approval it prints the plan and
//! changes nothing.

use crate::adapters::Generators;
use crate::plan_cmd::{Probed, plan_for, render};
use crate::{Ctx, load_resolved, paint, require_binding, state_cmd, usage};
use dfm_engine::{
    Approval as Allowance, Dir, Generator, Run, Store, Sys, apply, check_allowances,
    check_journal_capacity, gates, now_ns,
};
use dfm_model::display::{visible, visible_for_paste};
use dfm_model::plan::Scope;
use dfm_model::state::{
    Approval, BinaryId, DisruptiveApproval, ProfileRef, Receipt, RunResult, SCHEMA, SourceId,
    Stamp, Status, count_statuses, rfc3339,
};
use dfm_model::{Code, Exit, Refusal};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

#[derive(Clone)]
pub struct Args {
    /// `--root`; `None` takes the root the binding records.
    pub root: Option<String>,
    pub yes_plan: Option<String>,
    pub allow: BTreeSet<String>,
    pub allow_adopt: BTreeSet<String>,
    pub preview: bool,
    /// Which resources the run acts on: `apply` is `Config`, `secrets` is `Secrets`.
    pub scope: Scope,
    /// `packages --update`: a rolling update of the installed rolling tools
    /// (docs/toolchains.md) instead of an install of the missing ones.
    pub update: bool,
    /// `services --restart <resource-id>`: current services to plan as a disruptive restart.
    pub restart: BTreeSet<String>,
    /// A scope of `sync` (docs/sync.md): sync printed the plan and prints the result line, so
    /// this run prints neither, and its report omits the `not_in_scope` operations.
    pub(crate) composed: bool,
}

/// `apply [--root <dir>] [-y|--yes-plan <digest>] [--allow-disruptive <operation-id>]...
/// [--allow-adopt <operation-id>]... [--preview]`, and `secrets` with the same flags.
pub fn parse_args(args: &[String], scope: Scope) -> Result<Args, Refusal> {
    let (mut root, mut yes, mut preview, mut update) = (None, None, false, false);
    let (mut allow, mut allow_adopt) = (BTreeSet::new(), BTreeSet::new());
    let mut restart = BTreeSet::new();
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
            "--yes-plan" | "-y" => {
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
            "--update" if scope == Scope::Packages && !update => update = true,
            "--update" if update => return Err(usage("--update given twice")),
            "--restart" if scope == Scope::Services => {
                restart.insert(value(None)?);
            }
            other => return Err(usage(format!("unknown argument {other}"))),
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
        root,
        yes_plan: yes,
        allow,
        allow_adopt,
        preview,
        scope,
        update,
        restart,
        composed: false,
    })
}

/// Whether `d` has the shape of a plan digest: 64 lowercase hex characters.
pub(crate) fn is_digest(d: &str) -> bool {
    d.len() == 64
        && d.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

pub(crate) fn stale(approved: &str, now: &str) -> Refusal {
    Refusal::new(
        Code::PlanStale,
        format!(
            "the approved plan digest {approved} is not the digest of the plan for this machine now ({now}). Fix: read the new plan with --preview and approve its digest"
        ),
    )
}

pub(crate) fn label(s: Status) -> &'static str {
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

/// Every status, so the counts add up to the number of operations.
fn counts_line(counts: &BTreeMap<Status, u32>) -> String {
    const ORDER: [Status; 10] = [
        Status::Updated,
        Status::Current,
        Status::Failed,
        Status::Blocked,
        Status::Unknown,
        Status::NotStarted,
        Status::NotInScope,
        Status::Held,
        Status::Pending,
        Status::BlockedPending,
    ];
    let mut line = String::from("counts:");
    for s in ORDER {
        let n = counts.get(&s).copied().unwrap_or(0);
        line.push_str(&format!(" {}={n}", label(s)));
    }
    line
}

/// The checks between the printed plan and the writer lock, shared with `sync`: the approved
/// digest must be the plan's, the plan must have no blocker, the gates that need no write must
/// pass, the `--allow-*` sets must equal the plan's, and the journal must have room. A refusal
/// here changes nothing. The missing allowances are printed to paste. Returns whether the
/// journal holds earlier work that is unfinished or has an unknown outcome.
pub(crate) fn check_before_lock(
    ctx: &Ctx,
    args: &Args,
    plan: &dfm_model::plan::Plan,
    digest: &str,
    generator: &dyn Generator,
    out: &mut dyn Write,
) -> Result<bool, Refusal> {
    let w =
        |e: std::io::Error| Refusal::new(Code::IoFailed, format!("output failed: {}", e.kind()));
    // A digest that no longer matches is the first thing to report: the reviewer approved a
    // different plan. A blocked plan cannot be approved at all.
    if let Some(approved) = &args.yes_plan
        && *approved != digest
    {
        return Err(stale(approved, digest));
    }
    if let Some(b) = plan.blockers.first() {
        return Err(b.refusal());
    }
    // The gates that need no write run before the lock and in a preview, so a plan that apply
    // would refuse is refused here with the same code.
    let g = gates(plan, generator);
    if let Some(r) = g.refusal {
        return Err(r);
    }
    // An id that names no operation of its kind is a usage error and wins over a missing
    // approval. A missing approval is refused with every missing flag printed to paste.
    if let Err(all) = check_allowances(&g, &args.allow, &args.allow_adopt) {
        for e in all.iter().filter(|e| e.code != Code::Usage) {
            writeln!(out, "{}", visible_for_paste(&e.detail)).map_err(w)?;
        }
        return Err(all
            .into_iter()
            .next()
            .unwrap_or_else(|| usage("approval check failed")));
    }
    // The journal size is read without the lock. The lock is taken only for a real run.
    let (layout, sys) = (ctx.layout(), ctx.sys()?);
    let readable = Store::open_read(sys, &layout, state_cmd::fail_from_env()?)?
        .ok_or_else(crate::state_lost_refusal)?;
    let settling = readable.settlement_entries()?;
    check_journal_capacity(readable.journal_len()?, settling, plan)?;
    // An unknown outcome with no intent is not settled, yet apply blocks the operation it names.
    Ok(settling > 0 || !dfm_model::state::unresolved(&readable.read_journal_lenient()?).is_empty())
}

/// A run without `--yes-plan` whose plan writes nothing, with no open journal entry, has nothing
/// to approve: it ends here, exit 0, without the lock and without a receipt, as the approved run
/// would. Returns whether it ended.
pub(crate) fn end_without_change(
    args: &Args,
    plan: &dfm_model::plan::Plan,
    journal_open: bool,
    color: bool,
    out: &mut dyn Write,
) -> std::io::Result<bool> {
    if args.yes_plan.is_some() || journal_open || !plan.changes_nothing() {
        return Ok(false);
    }
    writeln!(
        out,
        "{}",
        paint::style(color, "1;32", "nothing to change: no approval needed")
    )?;
    writeln!(
        out,
        "{} {}",
        paint::label(color, "result:"),
        paint::status(color, "ok")
    )?;
    Ok(true)
}

/// The providers' answers a plan of `scope` needs (docs/packages.md, docs/lifecycle.md). The
/// probes are read-only and run only for their scope.
pub(crate) fn probe(
    ctx: &Ctx,
    sys: &Sys,
    args: &Args,
    resolved: &dfm_model::resolve::Resolved,
) -> Result<Probed, Refusal> {
    let mut probed = Probed::default();
    match args.scope {
        Scope::Packages if args.update => {
            probed.package = crate::adapters::package::update_facts(resolved, &ctx.runner(sys))?;
        }
        Scope::Packages => {
            probed.package = crate::adapters::package::package_facts(resolved, &ctx.runner(sys))?;
        }
        Scope::Services => {
            probed.service = crate::adapters::service::service_facts(resolved, &ctx.runner(sys))?;
            probed.restart = args.restart.clone();
        }
        _ => {}
    }
    Ok(probed)
}

/// A restart request must name a service of the plan, as an approval flag must name an
/// operation: a misspelled id would otherwise be ignored.
pub(crate) fn check_restart(
    restart: &BTreeSet<String>,
    plan: &dfm_model::plan::Plan,
) -> Result<(), Refusal> {
    match restart.iter().find(|r| {
        !plan
            .operations
            .iter()
            .any(|o| o.resource == **r && o.id.ends_with("@service"))
    }) {
        Some(bad) => Err(usage(format!(
            "--restart {bad} names no service in this plan"
        ))),
        None => Ok(()),
    }
}

/// What was printed so far, so a refusal prints its `result:` line exactly once.
#[derive(Default)]
struct Progress {
    plan_printed: bool,
    result_printed: bool,
}

/// The generators of one scope over the sources the plan was resolved from.
pub(crate) fn generators_for<'a>(
    ctx: &Ctx,
    sys: &Sys,
    scope: Scope,
    loaded: &'a crate::Loaded,
) -> Generators<'a> {
    match scope {
        Scope::Secrets | Scope::Packages | Scope::Services => {
            Generators::with_runner(&loaded.resolved, &loaded.sources, ctx.runner(sys))
        }
        Scope::Config => Generators::new(&loaded.resolved),
    }
}

pub fn run(ctx: &Ctx, args: &Args, out: &mut dyn Write) -> Result<(), Refusal> {
    let mut progress = Progress::default();
    let r = run_inner(ctx, args, out, &mut progress);
    if let Err(e) = &r
        && progress.plan_printed
        && !progress.result_printed
    {
        // The printed result is the exit the process returns, from the same refusal.
        let _ = writeln!(
            out,
            "{} {}",
            paint::label(ctx.color, "result:"),
            paint::status(ctx.color, e.exit().name())
        );
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
    let binding = require_binding(ctx)?;
    let root = args.root.as_deref().map(Path::new);
    // The one place the handlers are installed. A scope that starts a provider child (a session
    // check, a probe) arms them now, before that child, so a stop request ends the child instead
    // of leaving it running (docs/secrets.md, docs/packages.md). The checks below then turn a
    // recorded signal into exit 130 or 143 before anything is written.
    let arm = || dfm_engine::signal::install();
    let stop = || dfm_engine::signal::requested().map_or(Ok(()), Err);
    if args.scope != Scope::Config {
        arm()?;
    }
    // Read-only look first. A refusal here changes nothing, not even the lock file.
    let first = load_resolved(ctx, root)?;
    let (layout, sys) = (ctx.layout(), ctx.sys()?);
    // A packages or services plan needs the providers' answers first.
    let probes = |resolved| probe(ctx, &sys, args, resolved);
    let plan = plan_for(ctx, &first, args.scope, probes(&first.resolved)?)?;
    stop()?;
    check_restart(&args.restart, &plan)?;
    let digest = plan.digest()?;
    if !args.composed {
        if let Some(i) = &ctx.isolated {
            writeln!(out, "isolated: true {i}").map_err(w)?;
        }
        render(&plan, &digest, ctx.color, out).map_err(w)?;
        progress.plan_printed = true;
    }
    let generators = |loaded| generators_for(ctx, &sys, args.scope, loaded);
    let journal_open = check_before_lock(ctx, args, &plan, &digest, &generators(&first), out)?;
    // The credential gate (docs/secrets.md): a secrets run without a vault
    // session stops here, exit 3, before the lock and before any write. Nothing is rendered.
    if args.scope == Scope::Secrets {
        crate::adapters::secret::credentials(&plan, &first.resolved, &ctx.runner(&sys))?;
    }
    stop()?;
    if args.preview {
        return Ok(());
    }
    if end_without_change(args, &plan, journal_open, ctx.color, out).map_err(w)? {
        progress.result_printed = true;
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
    if !store.is_initialized() {
        return Err(store.stamp(crate::state_lost_refusal()));
    }
    store
        .require_identity(&binding)
        .map_err(|r| store.stamp(r))?;
    let second = load_resolved(ctx, root).map_err(|r| store.stamp(r))?;
    let facts = probes(&second.resolved).map_err(|r| store.stamp(r))?;
    let locked = plan_for(ctx, &second, args.scope, facts).map_err(|r| store.stamp(r))?;
    stop().map_err(|r| store.stamp(r))?;
    let locked_digest = locked.digest().map_err(|r| store.stamp(r))?;
    if locked_digest != digest {
        return Err(store.stamp(stale(approved, &locked_digest)));
    }
    let home = Dir::open_home(&sys, &layout.home).map_err(|r| store.stamp(r))?;
    let generator = generators(&second);
    let run = Run {
        store: &store,
        sys,
        home: &home,
        home_path: home.shown(),
        generator: &generator,
        allow: &args.allow,
        allow_adopt: &args.allow_adopt,
        fail: &fail,
    };
    // For `apply` the handlers go in here: after the lock and the second plan (which ran `id -u`)
    // and before the first mutation (settling writes the journal). `secrets` and `packages` armed
    // them before their first child process, above.
    if args.scope == Scope::Config {
        arm().map_err(|r| store.stamp(r))?;
    }
    fail.hit("apply-after-install")
        .map_err(|r| store.stamp(r))?;
    let started = now_ns().map_err(|r| store.stamp(r))?;
    // A signal that came before a pre-run check failed still decides the exit.
    let report = apply(&run, &locked)
        .map_err(|r| store.stamp(dfm_engine::signal::requested().unwrap_or(r)))?;
    let finished = now_ns().unwrap_or(started);

    for warning in &report.warnings {
        writeln!(out, "warning: {}", crate::bounded(warning)).map_err(w)?;
    }
    if report.repaired > 0 {
        writeln!(
            out,
            "repaired a torn final journal line ({} bytes removed)",
            report.repaired
        )
        .map_err(w)?;
    }
    let counts = count_statuses(&report.operations);
    let mutated = store.mutated();
    // One exit, derived from the first problem, is both printed and returned.
    let is_signal = |r: &Refusal| matches!(r.code, Code::Interrupted | Code::Terminated);
    // A signal that came during the run wins over any other stop (exit precedence, docs/apply.md).
    let mut stopped = dfm_engine::signal::requested()
        .or(report.stopped)
        .map(|r| if mutated { r.after_mutation() } else { r });
    let exit = stopped.as_ref().map_or(Exit::Ok, Refusal::exit);

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
                let kind = Allowance::of(op)?;
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
        let command = match args.scope {
            Scope::Config => "apply",
            Scope::Secrets => "secrets",
            Scope::Packages => "packages",
            Scope::Services => "services",
        };
        let receipt = Receipt {
            schema: SCHEMA,
            op_id: format!("{command}-{}-{}", &digest[..16], std::process::id()),
            command: command.into(),
            update: args.update,
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
                // The journal and the ledger stay the record. The run is a failure; the final
                // exit below takes the higher-ranked of this and the stop.
                receipt_error = Some(e.after_mutation());
            }
        }
    }

    // A signal that arrived while the receipt was written is not in the receipt. It still decides
    // the exit, and the report says so.
    let mut late = None;
    if !stopped.as_ref().is_some_and(is_signal)
        && let Some(r) = dfm_engine::signal::requested()
    {
        late = Some(r.code.as_str());
        stopped = Some(if mutated { r.after_mutation() } else { r });
    }

    for (op, rec) in locked.operations.iter().zip(&report.operations) {
        if args.composed && rec.status == Status::NotInScope {
            continue;
        }
        let reload = rec
            .reload
            .as_deref()
            .map_or(String::new(), |r| format!(" reload={r}"));
        let error = rec
            .error
            .as_deref()
            .map_or(String::new(), |e| format!(": {e}"));
        writeln!(
            out,
            "  {} {}{reload}{error}",
            paint::status(ctx.color, label(rec.status)),
            visible(&op.id)
        )
        .map_err(w)?;
    }
    writeln!(out, "{}", counts_line(&counts)).map_err(w)?;
    // An adoption publishes nothing, so only a published output earns this note. A service
    // operation records its own reload state (`reloaded` or `requested`) on its line.
    if report
        .operations
        .iter()
        .any(|r| r.reload.as_deref() == Some(dfm_engine::RELOAD_NOT_ATTEMPTED))
    {
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
    if let Some(c) = late {
        writeln!(
            out,
            "signal: {c} arrived after the receipt content was fixed, so the receipt does not record it"
        )
        .map_err(w)?;
    }
    // One value is both printed and returned. The higher-ranked of the stop and the receipt
    // failure wins, and the receipt failure wins a tie (exit precedence, docs/apply.md).
    let outcome = match (receipt_error, stopped) {
        (Some(e), Some(r)) if e.exit().combine(r.exit()) != e.exit() => Err(r),
        (Some(e), _) => Err(e),
        (None, Some(r)) => Err(r),
        (None, None) => Ok(()),
    };
    if !args.composed {
        let final_exit = outcome.as_ref().err().map_or(Exit::Ok, Refusal::exit);
        writeln!(
            out,
            "{} {}",
            paint::label(ctx.color, "result:"),
            paint::status(ctx.color, final_exit.name())
        )
        .map_err(w)?;
        progress.result_printed = true;
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_counts_line_names_every_status_and_sums_to_the_operation_count() {
        let counts: BTreeMap<Status, u32> = Status::ALL
            .iter()
            .zip(1u32..)
            .map(|(s, n)| (*s, n))
            .collect();
        let line = counts_line(&counts);
        for s in Status::ALL {
            assert!(line.contains(&format!(" {}=", label(s))), "{line}");
        }
        let sum: u32 = line
            .split(' ')
            .skip(1)
            .map(|kv| kv.split_once('=').unwrap().1.parse::<u32>().unwrap())
            .sum();
        assert_eq!(sum, counts.values().sum::<u32>(), "{line}");
        assert!(
            line.contains(" held=3 pending=4 blocked_pending=7"),
            "{line}"
        );
    }
}
