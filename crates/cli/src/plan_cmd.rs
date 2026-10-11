//! `plan`: read-only resolution, local observation and the typed plan (docs/planner.md).
//!
//! Reads the bound profile, the trusted sources in `--root`, the ownership ledger and the
//! local state of the claimed paths. It writes nothing, fetches nothing, authenticates nothing
//! and starts no process.

use crate::{Ctx, load_resolved, read_existing_binding, usage};
use dfm_engine::{Dir, Fail, Store, merge, observe};
use dfm_model::display::visible;
use dfm_model::plan::{
    self, Binary, Context, MAX_PLAN_BYTES, Owned, Plan, Saved, Stale, Status, source_identity,
};
use dfm_model::{Code, Refusal};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub struct Args {
    pub root: String,
    pub json: bool,
    pub against: Option<String>,
}

/// `plan --root <dir> [--json] [--against <saved plan>]`, flags in any order.
pub fn parse_args(args: &[String]) -> Result<Args, Refusal> {
    let (mut root, mut against, mut json) = (None, None, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" if !json => json = true,
            "--json" => return Err(usage("--json given twice")),
            "--root" | "--against" => {
                let slot = if a == "--root" {
                    &mut root
                } else {
                    &mut against
                };
                let v = it
                    .next()
                    .ok_or_else(|| usage(format!("{a} needs a value")))?;
                if slot.replace(v.clone()).is_some() {
                    return Err(usage(format!("{a} given twice")));
                }
            }
            other => return Err(usage(format!("unknown argument {other}"))),
        }
    }
    Ok(Args {
        root: root.ok_or_else(|| usage("--root is required"))?,
        json,
        against,
    })
}

/// SHA-256 of the running executable: the binary identity of section 9. Streamed, not loaded.
fn binary_identity() -> Result<Binary, Refusal> {
    let io = |e: std::io::Error| Refusal::new(Code::IoFailed, format!("binary identity: {e}"));
    let exe = std::env::current_exe().map_err(io)?;
    let mut f = fs::File::open(exe).map_err(io)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf).map_err(io)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(Binary {
        version: env!("CARGO_PKG_VERSION").into(),
        sha256: dfm_model::digest::hex(h.finalize().as_slice()),
    })
}

/// Reads a saved plan: a regular file, not a link, at most 16 MiB.
fn read_saved(path: &Path) -> Result<Vec<u8>, Refusal> {
    let shown = visible(&path.display().to_string());
    let io = |e: std::io::Error| {
        Refusal::new(
            Code::ReportUnavailable,
            format!("cannot read the saved plan {shown}: {}", e.kind()),
        )
    };
    let f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(dfm_platform_macos::O_NOFOLLOW | dfm_platform_macos::O_NONBLOCK)
        .open(path)
        .map_err(io)?;
    if !f.metadata().map_err(io)?.is_file() {
        return Err(Refusal::new(
            Code::ReportUnavailable,
            format!("{shown} is not a regular file"),
        ));
    }
    let mut buf = Vec::new();
    f.take(MAX_PLAN_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(io)?;
    Ok(buf)
}

/// Ledger outputs as paths relative to HOME.
fn owned(store: &Store, home: &Path) -> Result<Vec<Owned>, Refusal> {
    dfm_engine::owned_outputs(&store.read_ledger_lenient()?, home)
}

/// Builds the plan for the bound profile. Read-only.
pub fn make_plan(ctx: &Ctx, root: &Path) -> Result<Plan, Refusal> {
    let loaded = load_resolved(ctx, root)?;
    let layout = ctx.layout();
    let sys = ctx.sys()?;
    // Ownership decides what may be replaced. Without state it is unknown, so planning fails
    // closed instead of treating every existing file as the user's.
    let store =
        Store::open_read(sys, &layout, Fail::none())?.ok_or_else(crate::state_lost_refusal)?;
    store.require_identity(&loaded.binding)?;
    let ledger = owned(&store, &layout.home)?;

    let home = Dir::open_home(&sys, &layout.home)?;
    let reqs = plan::requests(&loaded.resolved, &ledger);
    let mut obs = observe(&sys, &home, &reqs)?;
    let more = observe(
        &sys,
        &home,
        &plan::followups(&loaded.resolved, &obs, &ledger),
    )?;
    merge(&mut obs, more);

    // Claims that overlap the manager's own paths, by the store's rules: the text scope (case
    // folded, so it holds for directories that do not exist yet) and filesystem identity.
    let mut reserved = BTreeMap::new();
    let mut unchecked = BTreeMap::new();
    let claimed = loaded.resolved.resources.values().flat_map(|r| {
        let dirs = r.dir_claims.iter().map(|d| &d.dir);
        r.claims.iter().chain(dirs)
    });
    // Ledger outputs are checked too: a removal candidate is a path the manager would touch.
    for rel in claimed.chain(ledger.iter().map(|o| &o.path)) {
        let abs = home.shown().join(rel);
        // An overlap is a reserved path (E_STATE_INVALID from the store). Any other failure
        // means the path could not be checked, so the plan reports an unknown fact
        // (E_OBSERVATION_UNKNOWN, exit 3: "a required prerequisite that is UNKNOWN",
        // architecture section 10). The planner reports a link loop or an unreadable parent
        // first, with its own code.
        if let Err(e) = store.check_claim(&abs.to_string_lossy()) {
            if e.code == Code::StateInvalid {
                reserved.insert(rel.clone(), visible(&e.detail));
            } else {
                unchecked.insert(rel.clone(), visible(&e.detail));
            }
        }
    }

    let root_str = loaded.root.to_str().ok_or_else(|| {
        usage("--root resolves to a path that is not UTF-8; point --root at a UTF-8 path")
    })?;
    let cx = Context {
        binary: binary_identity()?,
        profile: loaded.binding.profile(),
        machine_id: loaded.binding.machine_id(),
        sources: vec![source_identity(root_str, &loaded.sources)],
        root: root_str,
        ledger,
        reserved,
        unchecked,
    };
    plan::build(&loaded.resolved, &cx, &obs)
}

pub(crate) fn render(p: &Plan, digest: &str, out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(
        out,
        "plan: profile={} machine={} scope={}",
        visible(&p.profile),
        p.machine_id,
        p.scope
    )?;
    for s in &p.sources {
        writeln!(out, "source: {} {}", visible(&s.root), s.tree)?;
    }
    writeln!(out, "operations: {}", p.operations.len())?;
    for o in &p.operations {
        let status = match o.status {
            Status::Planned => "planned",
            Status::Current => "current",
            Status::NotInScope => "not_in_scope",
            Status::Blocked => "blocked",
        };
        writeln!(
            out,
            "  {status} {} [{}] {}",
            visible(&o.id),
            visible(&o.module),
            visible(&o.effect)
        )?;
        if !o.after.is_empty() {
            writeln!(out, "    after: {}", visible(&o.after.join(", ")))?;
        }
    }
    for b in &p.blockers {
        writeln!(out, "blocked: {}", b.refusal())?;
    }
    writeln!(out, "plan digest: {digest}")
}

const STALE_SHOWN: usize = 20;

fn print_stale(list: &[Stale], out: &mut dyn Write) -> std::io::Result<()> {
    for s in list.iter().take(STALE_SHOWN) {
        writeln!(
            out,
            "stale {}: {} was {} now {}",
            s.what.as_str(),
            visible(&s.subject),
            visible(&s.was),
            visible(&s.now)
        )?;
    }
    if list.len() > STALE_SHOWN {
        writeln!(
            out,
            "stale: {} more differences not shown ({} in total)",
            list.len() - STALE_SHOWN,
            list.len()
        )?;
    }
    Ok(())
}

pub fn run(ctx: &Ctx, args: &Args, out: &mut dyn Write) -> Result<(), Refusal> {
    let w =
        |e: std::io::Error| Refusal::new(Code::IoFailed, format!("output failed: {}", e.kind()));
    // The binding is read before anything else so an unbound machine is refused first.
    read_existing_binding(ctx)?;
    let old = match &args.against {
        Some(f) => Some(Saved::parse(&read_saved(&PathBuf::from(f))?)?),
        None => None,
    };
    let fresh = make_plan(ctx, Path::new(&args.root))?;
    let digest = fresh.digest()?;
    let diffs = match &old {
        Some(old) => plan::stale(&old.plan, &fresh)?,
        None => Vec::new(),
    };
    if args.json {
        // One JSON document on stdout, whatever the result. The differences travel inside it.
        let mut saved = fresh.saved()?;
        saved.isolated = ctx.isolated.clone();
        saved.stale = diffs.clone();
        out.write_all(&saved.to_json()?).map_err(w)?;
        writeln!(out).map_err(w)?;
    } else {
        if let Some(i) = &ctx.isolated {
            writeln!(out, "isolated: true {i}").map_err(w)?;
        }
        render(&fresh, &digest, out).map_err(w)?;
        print_stale(&diffs, out).map_err(w)?;
    }
    if let Some(first) = diffs.first() {
        return Err(Refusal::new(
            Code::PlanStale,
            visible(&format!(
                "{} changed: {} was {} now {}. Fix: review the new plan and approve it again; a plan whose inputs changed cannot be applied",
                first.what.as_str(),
                first.subject,
                first.was,
                first.now
            )),
        ));
    }
    if old.is_some() && !args.json {
        writeln!(out, "plan current: the saved plan matches").map_err(w)?;
    }
    match fresh.blockers.first() {
        Some(b) => Err(b.refusal()),
        None => Ok(()),
    }
}
