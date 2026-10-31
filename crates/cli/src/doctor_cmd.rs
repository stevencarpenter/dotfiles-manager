//! `doctor` and `status`: one read-only inspection, two views (docs/doctor.md).
//!
//! The inspection reads the binding, the state store, the lock file note, the resolved profile,
//! the plan and the owned outputs. The only programs it starts are the read-only package and
//! service probes of `packages` and `services`. It executes nothing found on PATH, fetches
//! nothing, takes no lock and writes nothing. A failing source gives its own row and the other
//! rows still appear.

use crate::{
    Ctx, Loaded, clip, header, load_resolved, no_root, plan_cmd, read_existing_binding, usage,
};
use dfm_engine::{
    Dir, Fail, Layout, Loaded as StateLoaded, Store, Sys, fsys::Listing, merge, observe,
    verify_identity,
};
use dfm_model::binding::Binding;
use dfm_model::doctor::{DoctorCode as D, Level, Report, Row, RowCode};
use dfm_model::fold;
use dfm_model::plan::{
    Action, Node, Observations, Operation, Owned, Ownership, PackageState, Plan, Request, Scope,
    ServiceFacts, ServiceState, Status, in_scope, link_fingerprint, sensitive_paths,
};
use dfm_model::resolve::ResolvedResource;
use dfm_model::state::{Class, JournalEntry, Ledger, Receipt, unfinished, unresolved};
use dfm_model::{Code, Refusal};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// The most names doctor reads from one directory when it looks for staged copies.
const STRAY_SCAN_MAX: usize = 100_000;

const DEFAULT_WIDTH: usize = 80;
const LOCK_NOTE_BYTES: u64 = 256;
const SHOWN_STRAYS: usize = 5;
/// The most links doctor follows from the executable PATH selects.
const LINK_HOPS: usize = 16;

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

/// Arguments are parsed first, so a usage error exits 2 whatever HOME is. A failed `ctx` is a
/// report: an ERROR row for the binding and UNKNOWN rows for the checks that need it (exit 1).
pub fn run(
    ctx: Result<Ctx, Refusal>,
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
    let (report, ctx) = match ctx {
        Ok(ctx) => (
            inspect(&ctx, command, args.root.as_deref().map(Path::new))?,
            Some(ctx),
        ),
        Err(r) => (no_context_report(command, &r), None),
    };
    if args.json {
        let json = report
            .to_json()
            .map_err(|e| Refusal::new(Code::IoFailed, format!("json output failed: {e}")))?;
        out.write_all(&json).map_err(w)?;
    } else {
        if let Some(ctx) = &ctx {
            header(out, ctx).map_err(w)?;
        }
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

/// The report when HOME could not be resolved: nothing else can be inspected.
fn no_context_report(command: &'static str, r: &Refusal) -> Report {
    let rows = vec![
        refusal_row("binding", "home directory", r),
        Row::new(
            "state",
            D::DependsUnknown,
            Level::Unknown,
            "state directory",
            "needs a usable home directory",
            "Fix the failed row above first.",
        ),
        Row::new(
            "profile",
            D::DependsUnknown,
            Level::Unknown,
            "bound profile",
            "needs a usable home directory",
            "Fix the failed row above first.",
        ),
    ];
    Report::new(command, None, rows)
}

/// The shared inspection. Fails only when no report can be made at all (exit 2).
fn inspect(ctx: &Ctx, command: &'static str, root: Option<&Path>) -> Result<Report, Refusal> {
    let sys = ctx
        .sys()
        .map_err(|r| Refusal::new(Code::ReportUnavailable, r.detail))?;
    let layout = ctx.layout();
    let mut rows = Vec::new();
    let binding = binding_row(ctx, &mut rows);
    // A schema 1 binding without --root is a usage error, as it was before the default.
    if root.is_none() && binding.as_ref().is_some_and(|b| b.root().is_none()) {
        return Err(no_root());
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let observed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let owned = state_rows(sys, &layout, binding.as_ref(), &mut rows);
    lock_row(ctx, sys.uid, &mut rows);
    let (loaded, plan) = profile_rows(ctx, &sys, &path, binding.as_ref(), root, &mut rows);
    output_rows(
        sys,
        &layout,
        owned.as_deref(),
        loaded.as_ref(),
        plan.as_ref(),
        &mut rows,
    );
    Ok(
        Report::new(command, ctx.isolated.clone(), rows)
            .observed(observed, &path.to_string_lossy()),
    )
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
        Code::HomeMismatch => {
            "Run from the account whose password-database home equals HOME and has no whitespace or control characters."
        }
        Code::ReportUnavailable => "Pass an existing --root directory.",
        Code::ObservationUnknown => "Fix the permissions or the path named here, then run doctor.",
        _ => "Fix the file named here, then run doctor.",
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
        r.code,
        status,
        subject,
        &clip(&r.detail),
        fix_for(r.code),
    )
}

fn ok_row(kind: &'static str, subject: &str, detail: &str) -> Row {
    Row::new(kind, D::Ok, Level::Ok, subject, detail, "")
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

// ---- state --------------------------------------------------------------------------------

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
                D::StateAbsent,
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
    let owned = ledger_rows(&store, layout, rows);
    let open = journal_rows(&store, rows);
    let paths = owned
        .iter()
        .flatten()
        .map(|o| o.path.clone())
        .chain(open.iter().filter_map(|p| {
            let rel = Path::new(p).strip_prefix(&layout.home).ok()?;
            Some(rel.to_str()?.to_string())
        }))
        .collect();
    staged_copy_rows(sys, layout, &paths, rows);
    let bad = evidence_rows(&l, &findings, rows);
    trial_rows(&l, &findings, rows);
    receipt_row(&l, bad, rows);
    upgrade_row(&l, rows);
    owned
}

/// The ledger row and the owned outputs. One path outside HOME gets its own ERROR row and does
/// not hide the others.
fn ledger_rows(store: &Store, layout: &Layout, rows: &mut Vec<Row>) -> Option<Vec<Owned>> {
    let led: Ledger = match store.read_ledger_lenient() {
        Ok(l) => l,
        Err(r) => {
            rows.push(refusal_row("state", "ledger.json", &r));
            return None;
        }
    };
    let total: usize = led.resources.iter().map(|r| r.owned_outputs.len()).sum();
    rows.push(ok_row(
        "state",
        "ledger.json",
        &format!("{} resources, {total} owned outputs", led.resources.len()),
    ));
    let mut owned = Vec::new();
    for r in &led.resources {
        for o in &r.owned_outputs {
            match Path::new(&o.path)
                .strip_prefix(&layout.home)
                .ok()
                .and_then(Path::to_str)
            {
                Some(rel) => owned.push(Owned {
                    resource: r.resource.clone(),
                    adapter: r.adapter.clone(),
                    class: r.class,
                    path: rel.into(),
                    fingerprint: o.fingerprint.clone(),
                    coowned_entries: o
                        .coowned
                        .as_ref()
                        .map(|c| (c.entries.len(), c.appended.len())),
                }),
                None => rows.push(refusal_row(
                    "output",
                    &o.path,
                    &Refusal::new(Code::StateInvalid, "the ledger path is outside HOME"),
                )),
            }
        }
    }
    Some(owned)
}

/// Adds the journal rows. Returns the paths of the intents and unknown entries that are not
/// settled.
fn journal_rows(store: &Store, rows: &mut Vec<Row>) -> Vec<String> {
    let entries = match store.read_journal_lenient() {
        Ok(e) => e,
        Err(r) => {
            rows.push(refusal_row("state", "journal.jsonl", &r));
            return Vec::new();
        }
    };
    let mut open = Vec::new();
    rows.push(ok_row(
        "state",
        "journal.jsonl",
        &format!("{} entries", entries.len()),
    ));
    for e in unfinished(&entries) {
        open.extend(
            e.recovery
                .iter()
                .flat_map(|r| &r.outputs)
                .map(|o| o.path.clone()),
        );
        let subject = e
            .recovery
            .iter()
            .flat_map(|r| &r.outputs)
            .map(|o| o.path.as_str())
            .next()
            .unwrap_or(&e.op);
        rows.push(Row::new(
            "intent",
            Code::OpInterrupted,
            Level::Error,
            subject,
            &format!(
                "operation {} of run {} (resource {}, began {}) has an intent and no outcome",
                e.op, e.op_id, e.resource, e.at
            ),
            &if e.op == "trial-adopt" {
                adopt_fix(e)
            } else {
                "Wait for a running apply to finish, then run apply. An apply that holds the lock reports E_LOCKED. Doctor never settles or replays.".to_string()
            },
        ));
    }
    for u in unresolved(&entries) {
        open.extend(u.paths().map(str::to_string));
        let id = &u.unknown.op_id;
        let subject = u.paths().next().unwrap_or(&u.unknown.op);
        let blocks = if u.paths().next().is_some() {
            "blocks this path in every run"
        } else {
            "has no intent record with a path, so it blocks every operation of its resource (only `--as failed` clears it)"
        };
        rows.push(Row::new(
            "intent",
            Code::OpInterrupted,
            Level::Error,
            subject,
            &format!(
                "operation {} (journal id {id}, resource {}, since {}) ended unknown and {blocks}",
                u.unknown.op, u.unknown.resource, u.unknown.at
            ),
            &if u.unknown.op == "trial-adopt" {
                adopt_fix(u.intent.unwrap_or(u.unknown))
            } else {
                format!(
                "Run `dfm state recover` or apply: they settle the entry when the path shows a clear result. Otherwise check the path, then run `dfm state resolve '{}' --as done`{} or `--as failed`. Doctor never settles.",
                id.replace('\'', "'\\''"),
                if u.paths().nth(1).is_some() {
                    " (this entry has more than one path and cannot be resolved as done)"
                } else {
                    ""
                }
            )
            },
        ));
    }
    open
}

/// The fix for an open or unknown `trial-adopt` entry (docs/adopt.md, "Interruption"). It names
/// no path, so neither apply nor recovery settles it: the same adoption run again closes it, or
/// the operator abandons it as failed. The module comes from the intent's token.
fn adopt_fix(e: &JournalEntry) -> String {
    let module = e
        .token
        .as_deref()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok())
        .and_then(|v| v["module"].as_str().map(str::to_string))
        .unwrap_or_else(|| "<module>".into());
    format!(
        "Run `dfm adopt {} --module {module}` again: it completes the interrupted adoption and closes this entry. To abandon the adoption, run `dfm state resolve '{}' --as failed`. Doctor never settles.",
        e.resource,
        e.op_id.replace('\'', "'\\''")
    )
}

/// Whether `entry` has the staged-name shape of an older build: `.<name>.<pid>.<n>.<nanos>.tmp`
/// or `.stage`. Current builds tag the name with a hash, which this does not match.
fn is_old_stage(entry: &str, name: &str) -> bool {
    let Some(rest) = entry
        .strip_prefix('.')
        .and_then(|e| e.strip_prefix(name))
        .and_then(|e| e.strip_prefix('.'))
    else {
        return false;
    };
    let Some((nums, suffix)) = rest.rsplit_once('.') else {
        return false;
    };
    let digits = |d: &str| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit());
    matches!(suffix, "tmp" | "stage") && nums.split('.').count() == 3 && nums.split('.').all(digits)
}

/// The most names doctor reads from one directory when it looks for staged copies. Only a
/// `test-isolation` build reads `DFM_STRAY_SCAN_MAX`.
#[cfg(feature = "test-isolation")]
fn stray_scan_max() -> usize {
    std::env::var("DFM_STRAY_SCAN_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(STRAY_SCAN_MAX)
}

#[cfg(not(feature = "test-isolation"))]
fn stray_scan_max() -> usize {
    STRAY_SCAN_MAX
}

/// A WARN row for each name beside an owned output or an open-intent path that looks like a
/// staged copy of an older build. Such a name is never swept, because a user file can have the
/// same shape. Doctor lists each directory once and reads no content. A listing that was cut
/// gets its own WARN row, because the scan of that directory is then partial. `rels` are
/// HOME-relative.
fn staged_copy_rows(sys: Sys, layout: &Layout, rels: &BTreeSet<String>, rows: &mut Vec<Row>) {
    let Ok(home) = Dir::open_home(&sys, &layout.home) else {
        return;
    };
    let mut by_dir: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for rel in rels {
        let (parent, name) = rel.rsplit_once('/').unwrap_or(("", rel));
        by_dir.entry(parent).or_default().push(name);
    }
    let max = stray_scan_max();
    for (parent, names) in by_dir {
        let mut dir: Option<Dir> = None;
        let mut reachable = true;
        for part in parent.split('/').filter(|p| !p.is_empty()) {
            match dir.as_ref().unwrap_or(&home).peek_dir(part) {
                Ok(Some(d)) => dir = Some(d),
                _ => {
                    reachable = false;
                    break;
                }
            }
        }
        if !reachable {
            continue;
        }
        let Ok(listing) = dir.as_ref().unwrap_or(&home).list_upto(max) else {
            continue;
        };
        rows.extend(scan_listing(parent, &names, &listing, max));
    }
}

/// The rows for one directory: a staged copy of an older build for each entry that matches any
/// of the owned `names` in it, and one row if the listing saw more than `max` entries (it was
/// cut). `parent` is HOME-relative and empty for HOME itself.
fn scan_listing(parent: &str, names: &[&str], listing: &Listing, max: usize) -> Vec<Row> {
    let shown = |entry: &str| {
        if parent.is_empty() {
            entry.to_string()
        } else {
            format!("{parent}/{entry}")
        }
    };
    let mut rows = Vec::new();
    if listing.seen() > max {
        rows.push(
            Row::new(
                "output",
                D::StagedCopy,
                Level::Warn,
                if parent.is_empty() { "." } else { parent },
                &format!(
                    "the directory has more than {max} entries; the scan for staged copies was cut"
                ),
                "Check the directory for names like .<name>.<digits>.<digits>.<digits>.tmp by hand.",
            )
            .partial(),
        );
    }
    for entry in listing.names.iter().take(max) {
        if let Some(name) = names.iter().find(|n| is_old_stage(entry, n)) {
            rows.push(Row::new(
                "output",
                D::StagedCopy,
                Level::Warn,
                &shown(entry),
                &format!(
                    "a name of an older staged-copy format beside {}",
                    clip(&shown(name))
                ),
                "A staged copy from an interrupted run; check it, then remove it by hand.",
            ));
        }
    }
    rows
}

/// Receipts, trials and stray files are audit evidence. Their failures are WARN rows and leave
/// the inventory partial. Returns how many rows it added.
fn evidence_rows(l: &StateLoaded, findings: &[dfm_engine::Finding], rows: &mut Vec<Row>) -> usize {
    let mut bad = 0;
    for f in findings
        .iter()
        .filter(|f| !["state.json", "ledger.json", "journal.jsonl"].contains(&f.file.as_str()))
    {
        bad += 1;
        rows.push(
            Row::new(
                "state",
                f.code,
                Level::Warn,
                &f.file,
                &clip(&f.detail),
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
        bad += 1;
        rows.push(
            Row::new(
                "state",
                D::StatePartial,
                Level::Warn,
                "state directory",
                &format!("{} stray entries: {}", l.strays.len(), shown.join(", ")),
                "Check the entries by hand. Doctor removes nothing.",
            )
            .partial(),
        );
    }
    bad
}

/// One row per active trial. An adopted trial has ended and has no row. A record that is
/// unreadable or written for another identity already has its WARN row among the evidence rows.
fn trial_rows(l: &StateLoaded, findings: &[dfm_engine::Finding], rows: &mut Vec<Row>) {
    for (n, t) in &l.trials {
        let Ok(t) = t else { continue };
        if !t.is_active() {
            continue;
        }
        let file = format!("trials/{n}");
        if findings.iter().any(|f| f.file == file) {
            continue;
        }
        let how = match &t.package {
            Some(p) if p.install.is_some() => {
                format!("{} installed by the trial with {}", p.name, p.adapter)
            }
            Some(p) => format!(
                "{} with {}, which existed before the trial",
                p.name, p.adapter
            ),
            None => "no install recorded".to_string(),
        };
        rows.push(Row::new(
            "trial",
            D::Trial,
            Level::Ok,
            &file,
            &format!(
                "trial {} of {} since unix {}: {how}; {} owned outputs",
                t.trial_id,
                t.tool,
                t.created_unix_ns / 1_000_000_000,
                t.owned_outputs.len()
            ),
            "",
        ));
    }
}

/// The newest readable receipt whose command is (`upgrade`) or is not (`!upgrade`) the upgrade
/// step's.
fn latest_receipt(l: &StateLoaded, upgrade: bool) -> Option<(&String, &Receipt)> {
    l.receipts
        .iter()
        .filter_map(|(n, r)| r.as_ref().ok().map(|r| (n, r)))
        .filter(|(_, r)| (r.command == "upgrade") == upgrade)
        .max_by(|a, b| a.0.cmp(b.0))
}

/// The last upgrade, apart from the last run, so a failed updater does not hide the result of
/// the last configuration run (docs/upgrade.md). No row before the first upgrade.
fn upgrade_row(l: &StateLoaded, rows: &mut Vec<Row>) {
    let Some((name, r)) = latest_receipt(l, true) else {
        return;
    };
    let detail = format!(
        "upgrade {} (exit {}) finished {}",
        r.result.outcome, r.result.exit_code, r.finished_at
    );
    rows.push(if r.result.outcome == "ok" {
        ok_row("upgrade", name, &detail)
    } else {
        Row::new(
            "upgrade",
            D::Upgrade,
            Level::Warn,
            name,
            &detail,
            "Run dfm sync.",
        )
    });
}

fn receipt_row(l: &StateLoaded, bad: usize, rows: &mut Vec<Row>) {
    let latest = latest_receipt(l, false);
    let unreadable = l.receipts.iter().filter(|r| r.1.is_err()).count();
    let row = match latest {
        None => Row::new(
            "receipt",
            D::NoReceipt,
            Level::NotApplicable,
            "receipts",
            "no readable receipt",
            "Run apply to record the first run.",
        ),
        Some((name, r)) => {
            let detail = format!(
                "{} {} (exit {}) finished {}",
                r.command, r.result.outcome, r.result.exit_code, r.finished_at
            );
            if r.result.outcome == "ok" {
                ok_row("receipt", name, &detail)
            } else {
                Row::new(
                    "receipt",
                    D::LastRun,
                    Level::Warn,
                    name,
                    &detail,
                    "Run plan to see what is still pending.",
                )
            }
        }
    };
    rows.push(if unreadable > 0 || bad > 0 {
        row.partial()
    } else {
        row
    });
}

// ---- lock ---------------------------------------------------------------------------------

/// The lock file note. Doctor never takes the lock, so a note only says who held it last. The
/// file must be a regular file owned by the user with one hard link and no group or other mode bits, or the row is a WARN.
fn lock_row(ctx: &Ctx, uid: u32, rows: &mut Vec<Row>) {
    let path = ctx.platform.lock_path();
    let shown = path.display().to_string();
    let warn = |why: &str| {
        Row::new(
            "lock",
            Code::StorageInvalid,
            Level::Warn,
            &shown,
            why,
            "Check that the lock file is a regular file owned by you, with mode 0600 and one hard link.",
        )
    };
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
            rows.push(warn(&format!("cannot open the lock file: {}", e.kind())));
            return;
        }
    };
    match f.metadata() {
        Ok(m) if m.is_file() && m.uid() == uid => {
            // The writer lock refuses both states. It opens the file read-write before it can
            // tighten anything, so a mode without the owner read or write bit fails there.
            let mode = m.mode() & 0o7777;
            if m.nlink() != 1 {
                rows.push(warn("the lock file has more than one hard link"));
                return;
            }
            if mode != 0o600 {
                rows.push(warn(&format!(
                    "the lock file mode is {mode:o}, expected 0600"
                )));
                return;
            }
        }
        Ok(_) => {
            rows.push(warn("the lock file is not a regular file owned by you"));
            return;
        }
        Err(e) => {
            rows.push(warn(&format!("cannot inspect the lock file: {}", e.kind())));
            return;
        }
    }
    let mut buf = Vec::new();
    if let Err(e) = f.take(LOCK_NOTE_BYTES).read_to_end(&mut buf) {
        rows.push(warn(&format!("cannot read the lock file: {}", e.kind())));
        return;
    }
    let note = String::from_utf8_lossy(&buf).trim().to_string();
    rows.push(if note.is_empty() {
        ok_row("lock", &shown, "the lock file has no holder note")
    } else {
        Row::new(
            "lock",
            D::LockNote,
            Level::Ok,
            &shown,
            &format!(
                "last holder note: {note}. Doctor does not take the lock, so it cannot tell whether a holder runs now"
            ),
            "",
        )
    });
}

// ---- profile and plan ---------------------------------------------------------------------

/// Profile, module, resource and plan rows. Returns what could be resolved and planned.
fn profile_rows(
    ctx: &Ctx,
    sys: &Sys,
    path: &OsStr,
    binding: Option<&Binding>,
    root: Option<&Path>,
    rows: &mut Vec<Row>,
) -> (Option<Loaded>, Option<Plan>) {
    let depends = |kind: &'static str, subject: &str, why: &str| {
        Row::new(
            kind,
            D::DependsUnknown,
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
        return (None, None);
    }
    let loaded = match load_resolved(ctx, root) {
        Ok(l) => l,
        Err(r) => {
            rows.push(refusal_row("profile", "bound profile", &r));
            rows.push(depends(
                "plan",
                "plan",
                "drift cannot be assessed without a resolved profile",
            ));
            return (None, None);
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
    // The read-only probes of `packages` and `services`; neither installs, loads or starts.
    // Only a stop request fails one, and then its resources keep the fallback row. The runner
    // creates its working directory on first use and resets its mode, and doctor writes nothing,
    // so unless that directory already exists with mode 0700 no provider starts and each package
    // and service is unknown.
    let runner = ctx.runner(sys);
    let private = std::fs::symlink_metadata(&runner.cwd)
        .is_ok_and(|m| m.is_dir() && m.permissions().mode() & 0o7777 == 0o700);
    let (packages, services) = if private {
        let packages = crate::adapters::package::package_facts(res, &runner).unwrap_or_else(|r| {
            rows.push(refusal_row("resource", "package probes", &r));
            BTreeMap::new()
        });
        let services = crate::adapters::service::service_facts(res, &runner, None, &loaded.root)
            .unwrap_or_else(|r| {
                rows.push(refusal_row("resource", "service probes", &r));
                BTreeMap::new()
            });
        (packages, services)
    } else {
        let why = format!(
            "the provider working directory {} does not exist yet or is not mode 0700, and doctor changes nothing",
            runner.cwd.display()
        );
        let of = |s: Scope| res.resources.values().filter(move |r| s.covers(r));
        (
            of(Scope::Packages)
                .map(|r| (r.id.clone(), PackageState::Unknown(why.clone())))
                .collect(),
            of(Scope::Services)
                .map(|r| {
                    let state = ServiceState::Unknown(why.clone());
                    (
                        r.id.clone(),
                        ServiceFacts {
                            state,
                            definition: None,
                        },
                    )
                })
                .collect(),
        )
    };
    for r in res.resources.values().filter(|r| !in_scope(r)) {
        if let (false, Some(s)) = (r.sensitive, packages.get(&r.id)) {
            rows.push(package_row(r, s, path));
            continue;
        }
        if let (false, Some(f)) = (r.sensitive, services.get(&r.id)) {
            rows.push(service_row(r, f));
            continue;
        }
        rows.push(if r.sensitive {
            Row::new(
                "resource",
                D::NaSensitive,
                Level::NotApplicable,
                &r.id,
                "sensitive resource: nothing is read",
                "",
            )
        } else {
            Row::new(
                "resource",
                D::ProbingNotBuilt,
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
    let plan = match plan_cmd::plan_for(
        ctx,
        &loaded,
        dfm_model::plan::Scope::Config,
        std::collections::BTreeMap::new(),
    ) {
        Ok(p) => Some(p),
        Err(r) => {
            rows.push(refusal_row("plan", "plan", &r));
            None
        }
    };
    (Some(loaded), plan)
}

/// A package row from the provider's read-only listing, with the executable PATH selects.
/// Another owner's copy is reported and never adopted.
fn package_row(r: &ResolvedResource, s: &PackageState, path: &OsStr) -> Row {
    let pkg = crate::adapters::package::name(&r.value).unwrap_or_else(|_| r.id.clone());
    let by = &r.adapter;
    let blocked = "Install it by hand with the provider, then run doctor.";
    let (status, code, detail, fix): (Level, RowCode, String, String) = match s {
        PackageState::Installed(v) | PackageState::Rolling(v) | PackageState::Held(v) => (
            Level::Ok,
            D::Ok.into(),
            format!("{pkg} is installed by {by} ({v})"),
            String::new(),
        ),
        PackageState::Missing(_) => (
            Level::Warn,
            D::PlanPending.into(),
            format!("{pkg} is not installed by {by}"),
            "Run dfm sync, which installs it.".into(),
        ),
        PackageState::OtherOwner(why) => (
            Level::Warn,
            Code::Conflict.into(),
            format!(
                "{pkg} is not installed by {by}, but {why}: an interfering installation, not adopted"
            ),
            "Remove the other installation, or declare the adapter that owns it.".into(),
        ),
        PackageState::Upgrades(d) => (
            Level::Warn,
            Code::Conflict.into(),
            format!(
                "{pkg} is not installed; installing it with {by} would upgrade {}",
                d.join(", ")
            ),
            blocked.into(),
        ),
        PackageState::Unofficial(why) => (
            Level::Warn,
            Code::SourceUntrusted.into(),
            why.clone(),
            blocked.into(),
        ),
        PackageState::SourceBuild(d) => (
            Level::Warn,
            Code::PlatformUnsupported.into(),
            format!(
                "{pkg} is not installed and {} has no prebuilt binary for this machine",
                d.join(", ")
            ),
            blocked.into(),
        ),
        PackageState::Unsupported(why) => (
            Level::Warn,
            Code::PrivilegeUnsupported.into(),
            format!("{pkg} is not installed and {by} cannot install it here: {why}"),
            blocked.into(),
        ),
        PackageState::Unknown(why) => (
            Level::Unknown,
            Code::ProbeFailed.into(),
            format!("the installed state of {pkg} with {by} is unknown: {why}"),
            "Make the provider answer its listing command, then run doctor.".into(),
        ),
        PackageState::Refused(why) => (
            Level::Error,
            Code::ManifestInvalid.into(),
            why.clone(),
            format!("Correct the resource value in {}.", r.declared_in),
        ),
    };
    let detail = format!(
        "{detail}; {}",
        selected(path, crate::adapters::package::leaf(&pkg))
    );
    Row::new("resource", code, status, &r.id, &clip(&detail), &fix).optional()
}

/// The executable `name` that the invoking process's PATH selects, and the links it resolves
/// through, at most [`LINK_HOPS`]. Nothing is executed, so a shim or stub is never run. A
/// relative PATH entry is skipped.
/// shortcut: the executable is taken to share the package's short name, as the owner check
/// does; upgrade when a resource declares its executables.
fn selected(path: &OsStr, name: &str) -> String {
    for dir in std::env::split_paths(path).filter(|d| d.is_absolute()) {
        let mut cur = dir.join(name);
        match std::fs::metadata(&cur) {
            Ok(m) if m.is_file() && m.permissions().mode() & 0o111 != 0 => {}
            _ => continue,
        }
        let mut chain = vec![cur.display().to_string()];
        for _ in 0..LINK_HOPS {
            let Ok(t) = std::fs::read_link(&cur) else {
                break;
            };
            cur = cur.parent().map_or_else(|| t.clone(), |p| p.join(&t));
            chain.push(cur.display().to_string());
        }
        return format!("PATH selects {}", chain.join(" -> "));
    }
    format!("PATH selects no executable named {name}")
}

/// A service row from the launcher's read-only report. A label loaded from another definition
/// is reported and never adopted.
fn service_row(r: &ResolvedResource, f: &ServiceFacts) -> Row {
    let by = &r.adapter;
    let runs = f
        .definition
        .as_ref()
        .map(|d| format!("; the definition runs {}", d.program))
        .unwrap_or_default();
    let (status, code, detail, fix): (Level, RowCode, String, String) = match &f.state {
        ServiceState::Current(d) => (Level::Ok, D::Ok.into(), format!("{by}: {d}"), String::new()),
        ServiceState::Stopped(d) => (
            Level::Warn,
            D::PlanPending.into(),
            format!("{by}: {d}"),
            "Run dfm sync, which starts it.".into(),
        ),
        ServiceState::Stale(d) => (
            Level::Warn,
            Code::Conflict.into(),
            format!("{by}: {d}"),
            format!("Run dfm sync --restart {} to restart it.", r.id),
        ),
        ServiceState::Foreign(d) => (
            Level::Warn,
            Code::Conflict.into(),
            format!("{by}: {d}: an interfering definition, not adopted"),
            "Unload it by hand with its launcher.".into(),
        ),
        ServiceState::Missing(why) => (
            Level::Warn,
            Code::Conflict.into(),
            format!("{by}: {why}"),
            "Put the prerequisite in place with dfm sync.".into(),
        ),
        ServiceState::Unknown(why) => (
            Level::Unknown,
            Code::ProbeFailed.into(),
            format!("the state of {by} is unknown: {why}"),
            "Make the launcher answer its report command, then run doctor.".into(),
        ),
        ServiceState::Refused(why) => (
            Level::Error,
            Code::ManifestInvalid.into(),
            why.clone(),
            format!("Correct the resource value in {}.", r.declared_in),
        ),
    };
    Row::new(
        "resource",
        code,
        status,
        &r.id,
        &clip(&format!("{detail}{runs}")),
        &fix,
    )
    .optional()
}

// ---- outputs ------------------------------------------------------------------------------

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

/// The facts for the owned outputs: the plan's observations where they serve, and one more
/// read-only pass for the paths they lack or did not hash. A sensitive path is never requested.
fn observe_owned(
    sys: Sys,
    layout: &Layout,
    owned: &[Owned],
    sensitive: &BTreeSet<String>,
    plan: Option<&Plan>,
) -> Result<Observations, Refusal> {
    let served = |p: &str| {
        plan.is_some_and(|pl| {
            pl.observations
                .seen
                .get(p)
                .is_some_and(|s| !matches!(s.node, Node::File { sha256: None, .. }))
        })
    };
    let reqs: Vec<Request> = owned
        .iter()
        .filter(|o| o.class == Class::Normal && !sensitive.contains(&fold(&o.path)))
        .filter(|o| !served(&o.path))
        .map(|o| Request {
            path: o.path.clone(),
            hash: true,
            list: false,
        })
        .collect();
    let mut obs = Observations::default();
    if !reqs.is_empty() {
        obs = observe(&sys, &Dir::open_home(&sys, &layout.home)?, &reqs)?;
    }
    if let Some(p) = plan {
        merge(&mut obs, p.observations.clone());
    }
    Ok(obs)
}

/// One row per owned output and per output the plan claims. A foreign file that the plan does
/// not claim has no row: a project-local override is not a conflict.
fn output_rows(
    sys: Sys,
    layout: &Layout,
    owned: Option<&[Owned]>,
    loaded: Option<&Loaded>,
    plan: Option<&Plan>,
    rows: &mut Vec<Row>,
) {
    let owned = owned.unwrap_or_default();
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
            b.code,
            status,
            subject,
            &clip(&format!("{} in {}: {}", b.resource, b.source, b.detail)),
            &clip(&b.correction),
        ));
    }
    let mut by_path: BTreeMap<&str, (Option<&Owned>, Option<&Operation>)> = BTreeMap::new();
    for o in owned {
        by_path.entry(&o.path).or_default().0 = Some(o);
    }
    for op in plan.iter().flat_map(|p| &p.operations) {
        if let Some(path) = op
            .path
            .as_deref()
            .filter(|_| op.action != Action::CreateDir)
        {
            let slot = by_path.entry(path).or_default();
            let held = slot.1.map(|h| h.resource.as_str());
            if takes_slot(slot.0.map(|o| o.resource.as_str()), held, &op.resource) {
                slot.1 = Some(op);
            }
        }
    }
    let sensitive = sensitive_paths(loaded.map(|l| &l.resolved), owned);
    let obs = observe_owned(sys, layout, owned, &sensitive, plan);
    for (path, (o, op)) in by_path {
        if blocked.contains(path) {
            continue;
        }
        if let Some(row) = classify(path, o, op, &sensitive, &obs) {
            rows.push(if plan.is_none() { row.partial() } else { row });
        }
    }
}

/// Whether an operation of `resource` replaces the one held for a path. A rename gives the path
/// two operations: the old owner's removal and the new owner's write. The row needs the new
/// owner's, whatever the plan order, so an operation of the ledger owner never displaces one of
/// another resource.
fn takes_slot(owner: Option<&str>, held: Option<&str>, resource: &str) -> bool {
    match held {
        None => true,
        Some(h) => owner != Some(resource) || owner == Some(h),
    }
}

/// The row for one path, or none when the plan has nothing to say about it.
fn classify(
    path: &str,
    o: Option<&Owned>,
    op: Option<&Operation>,
    sensitive: &BTreeSet<String>,
    obs: &Result<Observations, Refusal>,
) -> Option<Row> {
    let who = o
        .map(|o| o.resource.as_str())
        .or(op.map(|op| op.resource.as_str()))
        .unwrap_or("unknown");
    // Checked before anything else: a sensitive path is never looked at, whoever owns it.
    let is_sensitive = sensitive.contains(&fold(path))
        || o.is_some_and(|o| o.class == Class::Sensitive)
        || op.is_some_and(|op| op.class == Class::Sensitive);
    if is_sensitive {
        return Some(Row::new(
            "output",
            D::NaSensitive,
            Level::NotApplicable,
            path,
            &format!("resource {who}: sensitive output, content is not read"),
            "",
        ));
    }
    if let Some(o) = o {
        return Some(match obs {
            Err(r) => Row::new(
                "output",
                Code::ObservationUnknown,
                Level::Unknown,
                path,
                &format!(
                    "resource {who}: HOME cannot be observed: {}",
                    clip(&r.detail)
                ),
                fix_for(Code::ObservationUnknown),
            ),
            Ok(obs) => owned_row(path, who, op, live_state(o, obs)),
        });
    }
    let op = op?;
    match op.status {
        Status::Planned => Some(Row::new(
            "output",
            D::PlanPending,
            Level::Warn,
            path,
            &format!(
                "resource {who} claims it and does not own it yet: {}",
                op.effect
            ),
            if op.action == Action::Adopt {
                "Review plan, then run apply with --allow-adopt."
            } else {
                "Review plan, then run apply."
            },
        )),
        Status::Current if op.ownership == Ownership::UnrecordedCurrent => Some(Row::new(
            "output",
            D::Unrecorded,
            Level::Warn,
            path,
            &format!(
                "resource {who}: the output is the wanted result, but the ledger has no entry"
            ),
            "None needed: the manager leaves it alone and never removes it.",
        )),
        Status::Current => Some(ok_row(
            "output",
            path,
            &format!("resource {who}: already the wanted result"),
        )),
        Status::NotInScope | Status::Blocked | Status::Held => None,
    }
}

fn owned_row(path: &str, who: &str, op: Option<&Operation>, live: Live) -> Row {
    let row = |code: Code, level: Level, detail: &str| {
        Row::new("output", code, level, path, detail, fix_for(code))
    };
    match live {
        Live::Unknown(why) => row(
            Code::ObservationUnknown,
            Level::Unknown,
            &format!("resource {who}: {}", clip(&why)),
        ),
        Live::Parent(why) => row(
            Code::ParentUnsafe,
            Level::Error,
            &format!("resource {who}: {}", clip(&why)),
        ),
        Live::Missing => match op.filter(|op| op.status == Status::Planned && op.resource != who) {
            // A rename: the plan hands the path to another resource and retires this entry first.
            Some(op) => Row::new(
                "output",
                D::PlanPending,
                Level::Warn,
                path,
                &format!(
                    "owned by {who}; the output is gone and resource {} takes the path over: {}",
                    op.resource, op.effect
                ),
                "Review plan, then run apply.",
            ),
            None => Row::new(
                "output",
                D::OutputMissing,
                Level::Warn,
                path,
                &format!("owned by {who}; the output is gone"),
                "Run apply to publish it again.",
            ),
        },
        Live::Differs(why) => row(
            Code::Conflict,
            Level::Error,
            &format!("owned by {who}; {why}. The manager will not overwrite or remove it"),
        ),
        Live::Match => match op.filter(|op| op.status == Status::Planned) {
            Some(op) if op.action == Action::Remove => Row::new(
                "output",
                D::PlanRemoval,
                Level::Warn,
                path,
                &format!("owned by {who}; nothing selects it any more: {}", op.effect),
                "Review plan, then run apply with --allow-disruptive.",
            ),
            Some(op) => Row::new(
                "output",
                D::PlanPending,
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

#[cfg(test)]
mod tests {
    use super::{Listing, scan_listing, takes_slot};

    fn listing(names: &[&str], odd: usize) -> Listing {
        Listing {
            names: names.iter().map(|n| n.to_string()).collect(),
            odd,
        }
    }

    #[test]
    fn one_listing_is_tested_against_every_owned_name_in_the_directory() {
        let entries = listing(
            &[".a.1.2.3.tmp", ".b.4.5.6.stage", ".c.1.2.3.tmp", "a", "b"],
            0,
        );
        let rows = scan_listing("d", &["a", "b"], &entries, 100);
        let subjects: Vec<&str> = rows.iter().map(|r| r.subject.as_str()).collect();
        assert_eq!(subjects, ["d/.a.1.2.3.tmp", "d/.b.4.5.6.stage"]);
    }

    #[test]
    fn a_listing_longer_than_the_cap_adds_one_cut_row() {
        let rows = scan_listing("", &["x"], &listing(&["x", "y", "z"], 0), 2);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].detail.contains("was cut") && rows[0].partial);
    }

    #[test]
    fn names_that_are_not_utf8_count_toward_the_cap() {
        // `list_upto` stops on the same count, so a cut made by such names is reported too.
        let cut = listing(&["y"], 2);
        assert_eq!(cut.seen(), 3);
        let rows = scan_listing("", &["x"], &cut, 2);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].detail.contains("was cut") && rows[0].partial);
        assert!(scan_listing("", &["x"], &listing(&["y"], 1), 2).is_empty());
    }

    #[test]
    fn the_new_owner_wins_a_shared_path_in_either_plan_order() {
        let owner = Some("old");
        // Removal first, then the new owner's write.
        assert!(takes_slot(owner, None, "old"));
        assert!(takes_slot(owner, Some("old"), "new"));
        // The new owner's write first, then the removal: the removal does not displace it.
        assert!(takes_slot(owner, None, "new"));
        assert!(!takes_slot(owner, Some("new"), "old"));
        // No ledger owner: the last operation wins, as before.
        assert!(takes_slot(None, Some("a"), "b"));
    }
}
