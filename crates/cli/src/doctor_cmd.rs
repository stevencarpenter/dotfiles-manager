//! `doctor` and `status`: one read-only inspection, two views (docs/doctor.md).
//!
//! The inspection reads the binding, the state store, the lock file note, the resolved profile,
//! the plan and the owned outputs. It runs no program, probes no tool, fetches nothing, takes no
//! lock and writes nothing. A failing source gives its own row and the other rows still appear.

use crate::{Ctx, Loaded, bounded, header, load_resolved, plan_cmd, read_existing_binding, usage};
use dfm_engine::{Dir, Fail, Layout, Store, Sys, observe, owned_outputs, verify_identity};
use dfm_model::binding::Binding;
use dfm_model::doctor::{Level, Report, Row};
use dfm_model::plan::{
    Action, Node, Observations, Operation, Owned, Plan, Request, Status, in_scope, link_fingerprint,
};
use dfm_model::state::{Class, unfinished};
use dfm_model::{Code, Refusal};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const DEFAULT_WIDTH: usize = 80;
const LOCK_NOTE_BYTES: u64 = 256;
const SHOWN_STRAYS: usize = 5;

/// The table width. Only a `test-isolation` build reads `DFM_WIDTH`.
#[cfg(feature = "test-isolation")]
fn width() -> usize {
    std::env::var("DFM_WIDTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_WIDTH)
}

#[cfg(not(feature = "test-isolation"))]
fn width() -> usize {
    DEFAULT_WIDTH
}

pub fn run(
    ctx: &Ctx,
    command: &'static str,
    rest: &[String],
    out: &mut dyn Write,
) -> Result<(), Refusal> {
    let w =
        |e: std::io::Error| Refusal::new(Code::IoFailed, format!("output failed: {}", e.kind()));
    let args = plan_cmd::parse_args(rest)?;
    if args.against.is_some() {
        return Err(usage(format!("{command} does not take --against")));
    }
    let report = inspect(ctx, command, Path::new(&args.root))?;
    if args.json {
        let json = report
            .to_json()
            .map_err(|e| Refusal::new(Code::IoFailed, format!("json output failed: {e}")))?;
        out.write_all(&json).map_err(w)?;
    } else {
        header(out, ctx).map_err(w)?;
        write!(out, "{}", report.render(width(), command == "doctor")).map_err(w)?;
    }
    if report.failed() {
        return Err(Refusal::new(
            Code::Diagnosed,
            format!(
                "{} error rows, {} unknown rows",
                report.count(Level::Error),
                report.count(Level::Unknown)
            ),
        ));
    }
    Ok(())
}

/// The shared inspection. Fails only when no report can be made at all (exit 2).
fn inspect(ctx: &Ctx, command: &'static str, root: &Path) -> Result<Report, Refusal> {
    let sys = ctx
        .sys()
        .map_err(|r| Refusal::new(Code::ReportUnavailable, r.detail))?;
    let layout = ctx.layout();
    let mut rows = Vec::new();
    let binding = binding_row(ctx, &mut rows);
    let owned = state_rows(sys, &layout, binding.as_ref(), &mut rows);
    lock_row(ctx, &mut rows);
    let plan = profile_rows(ctx, binding.as_ref(), root, &mut rows);
    output_rows(sys, &layout, owned.as_deref(), plan.as_ref(), &mut rows);
    Ok(Report::new(command, ctx.isolated.clone(), rows))
}

fn fix_for(code: Code) -> &'static str {
    match code {
        Code::BindingInvalid => {
            "Run bind --profile <file> --machine-id <id>, or fix the binding file."
        }
        Code::StateInvalid | Code::StorageInvalid => {
            "Restore the state directory from backup. The manager does not repair or recreate state."
        }
        Code::IdentityMismatch => {
            "Restore the state of this identity, or bind the identity the state belongs to."
        }
        Code::ParentUnsafe => "Replace the linked parent with a real directory, then run plan.",
        Code::Conflict => "Review the file. Move it aside or restore it, then run plan.",
        Code::ReportUnavailable => "Pass an existing --root directory.",
        Code::ObservationUnknown => "Fix the permissions or the path named here, then run doctor.",
        _ => "Fix the file named here, then run doctor. See docs/doctor.md for the code.",
    }
}

/// A row for a source that failed. An unknown required fact is UNKNOWN, anything else is ERROR.
fn refusal_row(kind: &'static str, subject: &str, r: &Refusal) -> Row {
    let status = if r.code == Code::ObservationUnknown {
        Level::Unknown
    } else {
        Level::Error
    };
    Row::new(
        kind,
        r.code.as_str(),
        status,
        subject,
        &bounded(&r.detail),
        fix_for(r.code),
    )
}

fn ok_row(kind: &'static str, subject: &str, detail: &str) -> Row {
    Row::new(kind, "D_OK", Level::Ok, subject, detail, "None.")
}

fn binding_row(ctx: &Ctx, rows: &mut Vec<Row>) -> Option<Binding> {
    match read_existing_binding(ctx) {
        Ok(Some(b)) => {
            rows.push(ok_row(
                "binding",
                b.profile(),
                &format!("machine {}", b.machine_id()),
            ));
            Some(b)
        }
        Ok(None) => {
            let r = Refusal::new(Code::BindingInvalid, "this machine is not bound");
            rows.push(refusal_row("binding", "binding", &r));
            None
        }
        Err(r) => {
            rows.push(refusal_row("binding", "binding", &r));
            None
        }
    }
}

/// State store rows. Returns the owned outputs when the ledger could be read.
fn state_rows(
    sys: Sys,
    layout: &Layout,
    binding: Option<&Binding>,
    rows: &mut Vec<Row>,
) -> Option<Vec<Owned>> {
    let store = match Store::open_read(sys, layout, Fail::none()) {
        Ok(s) => s,
        Err(r) => {
            rows.push(refusal_row("state", "state directory", &r));
            return None;
        }
    };
    let (b, store) = match (binding, store) {
        (Some(b), Some(s)) => (b, s),
        (None, None) => {
            rows.push(Row::new(
                "state",
                "D_STATE_ABSENT",
                Level::NotApplicable,
                "state directory",
                "no state exists",
                "Run bind to create the binding and the state.",
            ));
            return None;
        }
        (Some(_), None) => {
            let r = Refusal::new(
                Code::StateInvalid,
                "the binding exists but its state directory is missing (state loss)",
            );
            rows.push(refusal_row("state", "state directory", &r));
            return None;
        }
        (None, Some(_)) => {
            let r = Refusal::new(
                Code::StateInvalid,
                "state exists but there is no readable binding",
            );
            rows.push(refusal_row("state", "state directory", &r));
            return None;
        }
    };
    let l = store.load();
    let findings = verify_identity(&l, b);
    match findings.iter().find(|f| f.file == "state.json") {
        Some(f) => rows.push(refusal_row(
            "state",
            "state.json",
            &Refusal::new(f.code, f.detail.clone()),
        )),
        None => rows.push(ok_row(
            "state",
            "state.json",
            "belongs to the bound identity",
        )),
    }

    // The lenient reads: one output with an unsafe parent must not hide the rest of the ledger.
    let mut owned = None;
    match store.read_ledger_lenient() {
        Ok(led) => {
            let outputs: usize = led.resources.iter().map(|r| r.owned_outputs.len()).sum();
            rows.push(ok_row(
                "state",
                "ledger.json",
                &format!("{} resources, {outputs} owned outputs", led.resources.len()),
            ));
            match owned_outputs(&led, &layout.home) {
                Ok(o) => owned = Some(o),
                Err(r) => rows.push(refusal_row("state", "ledger.json", &r)),
            }
        }
        Err(r) => rows.push(refusal_row("state", "ledger.json", &r)),
    }
    match store.read_journal_lenient() {
        Ok(entries) => {
            rows.push(ok_row(
                "state",
                "journal.jsonl",
                &format!("{} entries", entries.len()),
            ));
            for e in unfinished(&entries) {
                let subject = e
                    .recovery
                    .iter()
                    .flat_map(|r| &r.outputs)
                    .map(|o| o.path.as_str())
                    .next()
                    .unwrap_or(&e.op);
                rows.push(Row::new(
                    "intent",
                    Code::OpInterrupted.as_str(),
                    Level::Error,
                    subject,
                    &format!(
                        "operation {} of run {} (resource {}, began {}) has an intent and no outcome",
                        e.op, e.op_id, e.resource, e.at
                    ),
                    "Run apply. It settles each interrupted operation before new work. Doctor never settles or replays.",
                ));
            }
        }
        Err(r) => rows.push(refusal_row("state", "journal.jsonl", &r)),
    }

    // Receipts, trials and stray files are audit evidence. Their failures are WARN rows, and the
    // inventory they leave is partial.
    let mut bad = 0;
    for f in findings
        .iter()
        .filter(|f| !["state.json", "ledger.json", "journal.jsonl"].contains(&f.file.as_str()))
    {
        bad += 1;
        rows.push(
            Row::new(
                "state",
                f.code.as_str(),
                Level::Warn,
                &f.file,
                &bounded(&f.detail),
                "Audit evidence only: no ownership depends on it. Fix or remove it by hand.",
            )
            .partial(),
        );
    }
    if !l.strays.is_empty() {
        let shown: Vec<&str> = l
            .strays
            .iter()
            .take(SHOWN_STRAYS)
            .map(String::as_str)
            .collect();
        rows.push(
            Row::new(
                "state",
                "D_STATE_PARTIAL",
                Level::Warn,
                "state directory",
                &format!("{} stray entries: {}", l.strays.len(), shown.join(", ")),
                "Check the entries by hand. Doctor removes nothing.",
            )
            .partial(),
        );
    }
    let latest = l
        .receipts
        .iter()
        .filter_map(|(n, r)| r.as_ref().ok().map(|r| (n, r)))
        .max_by(|a, b| a.0.cmp(b.0));
    let unreadable = l.receipts.iter().filter(|r| r.1.is_err()).count();
    rows.push(match latest {
        None => {
            let r = Row::new(
                "receipt",
                "D_NO_RECEIPT",
                Level::NotApplicable,
                "receipts",
                "no readable receipt",
                "Run apply to record the first run.",
            );
            if unreadable > 0 { r.partial() } else { r }
        }
        Some((name, r)) => {
            let detail = format!(
                "{} {} (exit {}) finished {}",
                r.command, r.result.outcome, r.result.exit_code, r.finished_at
            );
            let row = if r.result.outcome == "ok" {
                ok_row("receipt", name, &detail)
            } else {
                Row::new(
                    "receipt",
                    "D_LAST_RUN",
                    Level::Warn,
                    name,
                    &detail,
                    "Run plan to see what is still pending.",
                )
            };
            if unreadable > 0 || bad > 0 {
                row.partial()
            } else {
                row
            }
        }
    });
    owned
}

/// The lock file note. Doctor never takes the lock, so a note only says who held it last.
fn lock_row(ctx: &Ctx, rows: &mut Vec<Row>) {
    let path = ctx.platform.lock_path();
    let shown = path.display().to_string();
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(dfm_platform_macos::O_NOFOLLOW | dfm_platform_macos::O_NONBLOCK)
        .open(&path);
    let f = match opened {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            rows.push(ok_row("lock", &shown, "no lock file"));
            return;
        }
        Err(e) => {
            rows.push(Row::new(
                "lock",
                Code::StorageInvalid.as_str(),
                Level::Warn,
                &shown,
                &format!("cannot read the lock file: {}", e.kind()),
                "Check that the lock file is a regular file owned by you.",
            ));
            return;
        }
    };
    let mut buf = Vec::new();
    let note = match f.take(LOCK_NOTE_BYTES).read_to_end(&mut buf) {
        Ok(_) => String::from_utf8_lossy(&buf).trim().to_string(),
        Err(e) => format!("unreadable ({})", e.kind()),
    };
    rows.push(Row::new(
        "lock",
        "D_LOCK_NOTE",
        Level::Ok,
        &shown,
        &format!("last holder note: {note}. Doctor does not take the lock, so it cannot tell whether a holder runs now"),
        "None.",
    ));
}

/// Profile, module, resource and plan rows. Returns the plan when it could be made.
fn profile_rows(
    ctx: &Ctx,
    binding: Option<&Binding>,
    root: &Path,
    rows: &mut Vec<Row>,
) -> Option<Plan> {
    let depends = |kind: &'static str, subject: &str, why: &str| {
        Row::new(
            kind,
            "D_DEPENDS_UNKNOWN",
            Level::Unknown,
            subject,
            why,
            "Fix the failed row above first.",
        )
    };
    if binding.is_none() {
        rows.push(depends(
            "profile",
            "bound profile",
            "needs a readable binding",
        ));
        return None;
    }
    let loaded: Loaded = match load_resolved(ctx, root) {
        Ok(l) => l,
        Err(r) => {
            rows.push(refusal_row("profile", "bound profile", &r));
            rows.push(depends(
                "plan",
                "plan",
                "drift cannot be assessed without a resolved profile",
            ));
            return None;
        }
    };
    let res = &loaded.resolved;
    rows.push(ok_row(
        "profile",
        &res.profile,
        &format!(
            "platform {} {}, {} modules, {} resources",
            res.platform,
            res.architecture,
            res.modules.len(),
            res.resources.len()
        ),
    ));
    for m in &res.modules {
        rows.push(ok_row("module", m, "selected"));
    }
    for r in res.resources.values().filter(|r| !in_scope(r)) {
        rows.push(if r.sensitive {
            Row::new(
                "resource",
                "D_NA_SENSITIVE",
                Level::NotApplicable,
                &r.id,
                "sensitive resource: nothing is read",
                "None.",
            )
        } else {
            Row::new(
                "resource",
                "D_PROBING_NOT_BUILT",
                Level::Unknown,
                &r.id,
                &format!(
                    "{} resource of module {}: installed state needs a probe",
                    r.kind, r.module
                ),
                "None in this build: probing is not built.",
            )
            .optional()
        });
    }
    match plan_cmd::plan_for(ctx, &loaded) {
        Ok(p) => Some(p),
        Err(r) => {
            rows.push(refusal_row("plan", "plan", &r));
            None
        }
    }
}

/// What the live entry at an owned path says about the ledger entry.
enum Live {
    Match,
    Missing,
    Differs(&'static str),
    Parent(String),
    Unknown(String),
}

fn live_state(o: &Owned, obs: &Observations) -> Live {
    let comps: Vec<&str> = o.path.split('/').collect();
    for i in 0..comps.len() {
        let p = comps[..=i].join("/");
        let last = i + 1 == comps.len();
        let Some(seen) = obs.seen.get(&p) else {
            return Live::Unknown(format!("{p} was not observed"));
        };
        match (&seen.node, last) {
            (Node::Dir { .. }, false) => {}
            (Node::Missing {}, _) => return Live::Missing,
            (Node::Unknown { reason }, _) => return Live::Unknown(reason.clone()),
            (_, false) => return Live::Parent(format!("{p} is not a real directory")),
            (
                Node::File {
                    sha256: Some(h), ..
                },
                true,
            ) => {
                return if o.fingerprint.as_deref() == Some(h.as_str()) {
                    Live::Match
                } else {
                    Live::Differs("its content differs from the recorded fingerprint")
                };
            }
            (Node::File { .. }, true) => {
                return Live::Unknown("the file content could not be hashed".into());
            }
            (Node::Link { target, .. }, true) => {
                return if o.fingerprint.as_deref() == Some(link_fingerprint(target).as_str()) {
                    Live::Match
                } else {
                    Live::Differs("it is not the link the manager published")
                };
            }
            (_, true) => return Live::Differs("it was replaced by another kind of entry"),
        }
    }
    Live::Unknown("empty path".into())
}

/// One row per owned output and per output the plan claims. A foreign file that the plan does
/// not claim has no row: a project-local override is not a conflict.
fn output_rows(
    sys: Sys,
    layout: &Layout,
    owned: Option<&[Owned]>,
    plan: Option<&Plan>,
    rows: &mut Vec<Row>,
) {
    let partial = plan.is_none();
    let mut blocked = BTreeSet::new();
    for b in plan.iter().flat_map(|p| &p.blockers) {
        let status = if b.code == Code::ObservationUnknown {
            Level::Unknown
        } else {
            Level::Error
        };
        let subject = b.path.as_deref().unwrap_or(&b.resource);
        blocked.insert(subject.to_string());
        rows.push(Row::new(
            "output",
            b.code.as_str(),
            status,
            subject,
            &bounded(&format!("{} in {}: {}", b.resource, b.source, b.detail)),
            &bounded(&b.correction),
        ));
    }

    let mut by_path: BTreeMap<&str, (Option<&Owned>, Option<&Operation>)> = BTreeMap::new();
    for o in owned.unwrap_or_default() {
        by_path.entry(&o.path).or_default().0 = Some(o);
    }
    for op in plan.iter().flat_map(|p| &p.operations) {
        if let Some(path) = op
            .path
            .as_deref()
            .filter(|_| op.action != Action::CreateDir)
        {
            by_path.entry(path).or_default().1 = Some(op);
        }
    }

    // Content is hashed only for normal outputs. A sensitive path is not even looked at.
    let reqs: Vec<Request> = owned
        .unwrap_or_default()
        .iter()
        .filter(|o| o.class == Class::Normal)
        .map(|o| Request {
            path: o.path.clone(),
            hash: true,
            list: false,
        })
        .collect();
    let obs = Dir::open_home(&sys, &layout.home).and_then(|home| observe(&sys, &home, &reqs));

    for (path, (o, op)) in by_path {
        if blocked.contains(path) {
            continue;
        }
        let sensitive = o.is_some_and(|o| o.class == Class::Sensitive)
            || op.is_some_and(|op| op.class == Class::Sensitive);
        let generated =
            op.is_some_and(|op| matches!(op.action, Action::CreateFile | Action::UpdateFile));
        let who = o
            .map(|o| o.resource.as_str())
            .or(op.map(|op| op.resource.as_str()));
        let who = who.unwrap_or("unknown");
        let mut row = if sensitive {
            Row::new(
                "output",
                "D_NA_SENSITIVE",
                Level::NotApplicable,
                path,
                &format!("resource {who}: sensitive output, content is not read"),
                "None.",
            )
        } else if generated {
            Row::new(
                "output",
                "D_NA_GENERATED",
                Level::NotApplicable,
                path,
                &format!("resource {who}: not applicable in this build (no generator)"),
                "None.",
            )
        } else if let Some(o) = o {
            match &obs {
                Err(r) => Row::new(
                    "output",
                    Code::ObservationUnknown.as_str(),
                    Level::Unknown,
                    path,
                    &format!(
                        "resource {who}: HOME cannot be observed: {}",
                        bounded(&r.detail)
                    ),
                    fix_for(Code::ObservationUnknown),
                ),
                Ok(obs) => owned_row(path, who, op, live_state(o, obs)),
            }
        } else if let Some(op) = op {
            match op.status {
                Status::Planned => Row::new(
                    "output",
                    "D_PLAN_PENDING",
                    Level::Warn,
                    path,
                    &format!(
                        "resource {who} claims it and does not own it yet: {}",
                        op.effect
                    ),
                    "Review plan, then run apply.",
                ),
                Status::Current
                    if op.ownership == dfm_model::plan::Ownership::UnrecordedCurrent =>
                {
                    Row::new(
                        "output",
                        "D_UNRECORDED",
                        Level::Warn,
                        path,
                        &format!(
                            "resource {who}: the output is the wanted result, but the ledger has no entry"
                        ),
                        "None: the manager leaves it alone and never removes it.",
                    )
                }
                Status::Current => ok_row(
                    "output",
                    path,
                    &format!("resource {who}: already the wanted result"),
                ),
                Status::NotInScope | Status::Blocked => continue,
            }
        } else {
            continue;
        };
        if partial {
            row = row.partial();
        }
        rows.push(row);
    }
}

fn owned_row(path: &str, who: &str, op: Option<&Operation>, live: Live) -> Row {
    match live {
        Live::Unknown(why) => Row::new(
            "output",
            Code::ObservationUnknown.as_str(),
            Level::Unknown,
            path,
            &format!("resource {who}: {}", bounded(&why)),
            fix_for(Code::ObservationUnknown),
        ),
        Live::Parent(why) => Row::new(
            "output",
            Code::ParentUnsafe.as_str(),
            Level::Error,
            path,
            &format!("resource {who}: {}", bounded(&why)),
            fix_for(Code::ParentUnsafe),
        ),
        Live::Missing => Row::new(
            "output",
            "D_OUTPUT_MISSING",
            Level::Warn,
            path,
            &format!("owned by {who}; the output is gone"),
            "Run apply to publish it again.",
        ),
        Live::Differs(why) => Row::new(
            "output",
            Code::Conflict.as_str(),
            Level::Error,
            path,
            &format!("owned by {who}; {why}. The manager will not overwrite or remove it"),
            fix_for(Code::Conflict),
        ),
        Live::Match => match op.filter(|op| op.status == Status::Planned) {
            Some(op) if op.action == Action::Remove => Row::new(
                "output",
                "D_PLAN_REMOVAL",
                Level::Warn,
                path,
                &format!("owned by {who}; nothing selects it any more: {}", op.effect),
                "Review plan, then run apply with --allow-disruptive.",
            ),
            Some(op) => Row::new(
                "output",
                "D_PLAN_PENDING",
                Level::Warn,
                path,
                &format!("owned by {who}; the wanted result changed: {}", op.effect),
                "Review plan, then run apply.",
            ),
            None => ok_row(
                "output",
                path,
                &format!("owned by {who}; matches the ledger fingerprint"),
            ),
        },
    }
}
