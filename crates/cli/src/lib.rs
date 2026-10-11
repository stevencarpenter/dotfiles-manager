//! `dotfiles-manager` command line: version, help, explicit profile binding and self-check.

mod adapters;
mod apply_cmd;
mod controls_fs;
mod doctor_cmd;
mod plan_cmd;
mod source_fs;
mod state_cmd;

pub use adapters::builtin_catalog;
pub use controls_fs::{check_build_controls, toolchain_tree_digest};

use dfm_engine::{Dir, Fail, Layout, Store, Sys, open_config};
use dfm_model::binding::{Binding, MAX_BYTES, check_profile};
use dfm_model::resolve::resolve;
use dfm_model::state::Stamp;
use dfm_model::{Code, Exit, Refusal};
use dfm_platform_macos::{Platform, check_supported, current_platform};
use source_fs::load_source_set;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const HELP: &str = "\
dotfiles-manager: bind this machine to one dotfiles profile.

usage:
  dotfiles-manager --version | version
  dotfiles-manager --help | help
  dotfiles-manager bind --profile <file> --machine-id <id>
  dotfiles-manager binding
  dotfiles-manager state check
  dotfiles-manager explain <resource-or-target> --root <dir>
  dotfiles-manager plan --root <dir> [--json] [--against <saved plan>]
  dotfiles-manager doctor --root <dir> [--json]
  dotfiles-manager status --root <dir> [--json]
  dotfiles-manager apply --root <dir> [--preview] [--yes-plan <digest>] [--allow-disruptive <operation-id>]...
  dotfiles-manager self-check --build-controls <staged tree>
  dotfiles-manager self-check --fixture-dir <dir>

bind records the profile and machine identity once. A second bind with a
different profile or machine is refused (E_REBIND_UNSUPPORTED). No command
infers a profile. Only apply changes configuration files.

explain resolves the bound profile against the trusted sources in --root and prints
ownership, permitted changes, provenance and native precedence for one resource or
output path. It reads only --root and the binding, and writes nothing.

plan resolves the bound profile against --root, observes the claimed paths under HOME
read-only, and prints the typed plan with its digest. It writes nothing, fetches nothing and
authenticates nothing. --json prints the plan for review. --against compares a saved --json
plan with a fresh one and exits 3 (E_PLAN_STALE) when any input changed.

doctor and status inspect without changing anything: the binding, the state store, the lock
note, the resolved profile, the plan and each owned output. They run no program, probe no tool,
fetch nothing, take no lock and write nothing. Each row has a status (OK, WARN, ERROR, UNKNOWN,
N/A), a stable code, the path or resource and one line of fix text. doctor prints detail and fix
for every row, status prints one block per row. --json prints the same rows as one document
(schema 1). Exit 0 when no row is ERROR and no required row is UNKNOWN, 1 otherwise, 2 for usage.
See docs/doctor.md.

apply plans again, prints the plan and exits 3 (E_APPROVAL_REQUIRED) unless --yes-plan carries the
digest of that exact plan. Then it publishes file links and removes previously owned outputs,
journals each step and writes a receipt. A removal also needs --allow-disruptive <operation-id>.
--preview prints the plan and its digest and changes nothing.

exit codes: 0 ok, 1 failed, 2 usage, 3 blocked before mutation, 4 declined,
5 pending, 130 interrupted, 143 terminated.
";

const MAX_ERROR_CHARS: usize = 512;

/// Control, invisible and bidirectional characters become visible escapes, and the length is
/// bounded.
fn bounded(s: &str) -> String {
    dfm_model::display::visible(&s.chars().take(MAX_ERROR_CHARS).collect::<String>())
}

struct Ctx {
    platform: Platform,
    binding_path: PathBuf,
    os: String,
    arch: String,
    isolated: Option<String>,
}

impl Ctx {
    fn layout(&self) -> Layout {
        Layout {
            home: self.platform.home().to_path_buf(),
            config_dir: self
                .binding_path
                .parent()
                .map_or_else(|| self.platform.config_dir(), Path::to_path_buf),
            state_root: self.platform.state_dir(),
            lock_file: self.platform.lock_path(),
            data_dir: self.platform.data_dir(),
            cache_dir: self.platform.cache_dir(),
        }
    }

    fn sys(&self) -> Result<Sys, Refusal> {
        let uid = dfm_platform_macos::current_uid()
            .ok_or_else(|| Refusal::new(Code::StorageInvalid, "cannot read the user id"))?;
        Ok(Sys { uid })
    }

    /// The binding file name inside the config directory.
    fn binding_name(&self) -> Result<String, Refusal> {
        self.binding_path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
            .ok_or_else(|| Refusal::new(Code::BindingInvalid, "the binding path has no file name"))
    }
}

#[cfg(feature = "test-isolation")]
fn context(home: Option<String>) -> Result<Ctx, Refusal> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let home = home.or_else(|| env("DFM_HOME"));
    let platform = match &home {
        Some(h) if !dfm_platform_macos::valid_home(h) => {
            return Err(Refusal::new(
                Code::HomeMismatch,
                "the home directory contains whitespace or control characters; this is not supported",
            ));
        }
        Some(h) => Platform::with_home(PathBuf::from(h)),
        None => Platform::detect()?,
    };
    let binding_path = env("DFM_BINDING").map_or_else(|| platform.binding_path(), PathBuf::from);
    let (os, arch) = match env("DFM_PLATFORM") {
        Some(p) => {
            let (o, a) = p.split_once('/').unwrap_or((&p, ""));
            (o.to_string(), a.to_string())
        }
        None => (
            current_platform().0.to_string(),
            current_platform().1.to_string(),
        ),
    };
    let isolated = Some(format!(
        "home={} binding={}",
        platform.home().display(),
        binding_path.display()
    ));
    Ok(Ctx {
        platform,
        binding_path,
        os,
        arch,
        isolated,
    })
}

#[cfg(not(feature = "test-isolation"))]
fn context(_home: Option<String>) -> Result<Ctx, Refusal> {
    let platform = Platform::detect()?;
    let (os, arch) = current_platform();
    Ok(Ctx {
        binding_path: platform.binding_path(),
        platform,
        os: os.into(),
        arch: arch.into(),
        isolated: None,
    })
}

#[cfg(feature = "test-isolation")]
fn split_home(args: &[String]) -> Result<(Option<String>, &[String]), Refusal> {
    match args {
        [flag, home, rest @ ..] if flag == "--home" => Ok((Some(home.clone()), rest)),
        [flag] if flag == "--home" => Err(usage("--home needs a value")),
        _ => Ok((None, args)),
    }
}

#[cfg(not(feature = "test-isolation"))]
fn split_home(args: &[String]) -> Result<(Option<String>, &[String]), Refusal> {
    Ok((None, args))
}

fn usage(detail: impl Into<String>) -> Refusal {
    Refusal::new(Code::Usage, detail)
}

/// Parses `--flag value` pairs. Unknown flags, repeats and missing values are usage errors.
fn flags<const N: usize>(
    args: &[String],
    names: [&str; N],
) -> Result<[Option<String>; N], Refusal> {
    let mut vals: [Option<String>; N] = std::array::from_fn(|_| None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let i = names
            .iter()
            .position(|n| n == a)
            .ok_or_else(|| usage(format!("unknown argument {a}")))?;
        let v = it
            .next()
            .ok_or_else(|| usage(format!("{a} needs a value")))?;
        if vals[i].replace(v.clone()).is_some() {
            return Err(usage(format!("{a} given twice")));
        }
    }
    Ok(vals)
}

/// Runs one invocation. Output goes to `out`; errors go to `err` as one bounded line.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> Exit {
    match dispatch(args, out) {
        Ok(()) => Exit::Ok,
        Err(r) => {
            let _ = writeln!(err, "error: {}", bounded(&r.to_string()));
            r.exit()
        }
    }
}

fn dispatch(args: &[String], out: &mut dyn Write) -> Result<(), Refusal> {
    let (home, args) = split_home(args)?;
    let w =
        |e: std::io::Error| Refusal::new(Code::IoFailed, format!("output failed: {}", e.kind()));
    let Some((cmd, rest)) = args.split_first() else {
        return write!(out, "{HELP}").map_err(w);
    };
    match cmd.as_str() {
        "--help" | "-h" | "help" if rest.is_empty() => write!(out, "{HELP}").map_err(w),
        "--version" | "-V" | "version" if rest.is_empty() => {
            let commit = option_env!("DFM_BUILD_COMMIT").unwrap_or("unknown");
            let iso = if cfg!(feature = "test-isolation") {
                " test-isolation"
            } else {
                ""
            };
            writeln!(
                out,
                "dotfiles-manager {} (commit {commit}){iso}",
                env!("CARGO_PKG_VERSION")
            )
            .map_err(w)
        }
        "bind" => {
            let [profile, id] = flags(rest, ["--profile", "--machine-id"])?;
            let ctx = context(home)?;
            header(out, &ctx).map_err(w)?;
            let msg = bind(
                &ctx,
                profile.ok_or_else(|| usage("--profile is required"))?,
                id.ok_or_else(|| usage("--machine-id is required"))?,
            )?;
            writeln!(out, "{msg}").map_err(w)
        }
        "binding" if rest.is_empty() => {
            let ctx = context(home)?;
            header(out, &ctx).map_err(w)?;
            match read_existing_binding(&ctx)? {
                None => writeln!(out, "unbound"),
                Some(b) => writeln!(
                    out,
                    "bound profile={} machine_id={}",
                    dfm_model::display::visible(b.profile()),
                    b.machine_id()
                ),
            }
            .map_err(w)
        }
        "state" => state_cmd::dispatch_state(&context(home)?, rest, out),
        #[cfg(feature = "test-isolation")]
        "__record-op" => state_cmd::record_op(&context(home)?, rest, out),
        #[cfg(feature = "test-isolation")]
        "__record-trial" => state_cmd::record_trial(&context(home)?, rest, out),
        #[cfg(feature = "test-isolation")]
        "__reconcile" if rest.is_empty() => state_cmd::reconcile(&context(home)?, out),
        "explain" => {
            let (query, root) = explain_args(rest)?;
            let ctx = context(home)?;
            header(out, &ctx).map_err(w)?;
            let text = explain(&ctx, &query, Path::new(&root))?;
            write!(out, "{text}").map_err(w)
        }
        "plan" => {
            let args = plan_cmd::parse_args(rest)?;
            plan_cmd::run(&context(home)?, &args, out)
        }
        "doctor" | "status" => {
            let name = if cmd == "doctor" { "doctor" } else { "status" };
            doctor_cmd::run(&context(home)?, name, rest, out)
        }
        "apply" => {
            let args = apply_cmd::parse_args(rest)?;
            apply_cmd::run(&context(home)?, &args, out)
        }
        "self-check" => {
            let [controls, fixture] = flags(rest, ["--build-controls", "--fixture-dir"])?;
            match (controls, fixture) {
                (Some(tree), None) => {
                    let ctx = context(home)?;
                    header(out, &ctx).map_err(w)?;
                    check_build_controls(&ctx.platform, Path::new(&tree))?;
                    writeln!(out, "build controls ok").map_err(w)
                }
                (None, Some(dir)) => {
                    fixture_check(Path::new(&dir))?;
                    writeln!(out, "self-check ok").map_err(w)
                }
                _ => Err(usage(
                    "self-check needs exactly one of --build-controls or --fixture-dir",
                )),
            }
        }
        other => Err(usage(format!(
            "unknown command or argument {other}; try --help"
        ))),
    }
}

/// `explain <query> --root <dir>`, in either order.
fn explain_args(args: &[String]) -> Result<(String, String), Refusal> {
    let mut query = None;
    let mut root = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--root" {
            let v = it.next().ok_or_else(|| usage("--root needs a value"))?;
            if root.replace(v.clone()).is_some() {
                return Err(usage("--root given twice"));
            }
        } else if a.starts_with("--") {
            return Err(usage(format!("unknown argument {a}")));
        } else if query.replace(a.clone()).is_some() {
            return Err(usage("explain takes one resource or output path"));
        }
    }
    Ok((
        query.ok_or_else(|| usage("explain needs a resource or output path"))?,
        root.ok_or_else(|| usage("--root is required"))?,
    ))
}

/// The bound profile resolved against the trusted sources in `root`.
struct Loaded {
    binding: Binding,
    /// The canonical `--root`.
    root: PathBuf,
    sources: dfm_model::resolve::SourceSet,
    resolved: dfm_model::resolve::Resolved,
}

/// Resolves the bound profile against `root`. Read-only.
fn load_resolved(ctx: &Ctx, root: &Path) -> Result<Loaded, Refusal> {
    let binding = read_existing_binding(ctx)?.ok_or_else(|| {
        Refusal::new(
            Code::BindingInvalid,
            "this machine is not bound; run bind first",
        )
    })?;
    let root = fs::canonicalize(root).map_err(|e| {
        Refusal::new(
            Code::ReportUnavailable,
            dfm_model::display::visible(&format!("{}: {}", root.display(), e.kind())),
        )
    })?;
    let rel = Path::new(binding.profile())
        .strip_prefix(&root)
        .ok()
        .and_then(Path::to_str)
        .ok_or_else(|| {
            Refusal::new(
                Code::ProfileInvalid,
                "the bound profile is not inside --root; point --root at the trusted source tree that contains it",
            )
        })?;
    let sources = load_source_set(&root)?;
    let resolved = resolve(&sources, rel, &builtin_catalog())?;
    Ok(Loaded {
        binding,
        root,
        sources,
        resolved,
    })
}

/// Resolves the bound profile against `root` and explains one resource. Read-only.
fn explain(ctx: &Ctx, query: &str, root: &Path) -> Result<String, Refusal> {
    let loaded = load_resolved(ctx, root)?;
    dfm_model::resolve::explain(&loaded.resolved, query)
}

fn header(out: &mut dyn Write, ctx: &Ctx) -> std::io::Result<()> {
    match &ctx.isolated {
        Some(v) => writeln!(out, "isolated: true {v}"),
        None => Ok(()),
    }
}

/// Reads and parses the binding file `name` in `dir`. The file is opened without following a
/// link and must be a regular file owned by the current user with mode 0600, at most 1 MiB.
/// `None` when it does not exist.
fn read_binding(sys: &Sys, dir: &Dir, name: &str) -> Result<Option<Binding>, Refusal> {
    let shown = dir.shown().join(name).display().to_string();
    match dir.read_private(sys, name, MAX_BYTES) {
        Ok(None) => Ok(None),
        Ok(Some(buf)) => Binding::parse(&buf)
            .map(Some)
            .map_err(|r| Refusal::new(r.code, format!("{shown}: {}", r.detail))),
        // A problem with the file itself (link, type, owner, mode) is a binding problem.
        Err(r) if r.code == Code::StorageInvalid => {
            Err(Refusal::new(Code::BindingInvalid, r.detail))
        }
        Err(r) => Err(r),
    }
}

fn rebind_refusal(have: &Binding) -> Refusal {
    Refusal::new(
        Code::RebindUnsupported,
        format!(
            "already bound to {} as {}; rebinding is an ownership migration and is not supported",
            have.profile(),
            have.machine_id()
        ),
    )
}

/// Opens the binding's directory (a redirected, mistyped or badly owned one is
/// `E_STORAGE_INVALID`), then reads the binding. Nothing is created.
fn read_existing_binding(ctx: &Ctx) -> Result<Option<Binding>, Refusal> {
    let sys = ctx.sys()?;
    match open_config(&sys, &ctx.layout(), false)? {
        None => Ok(None),
        Some(dir) => read_binding(&sys, &dir, &ctx.binding_name()?),
    }
}

/// A binding with no state directory is state loss, not a fresh start. Only a bind that finds no
/// binding may create state (docs/state.md).
fn state_lost_refusal() -> Refusal {
    Refusal::new(
        Code::StateInvalid,
        "the binding exists but its state directory is missing; state is not recreated (state loss). \
         Restore ~/.local/state/dotfiles-manager/, or move the binding aside to start new state",
    )
}

fn state_owner_refusal(owner: &Stamp) -> Refusal {
    Refusal::new(
        Code::RebindUnsupported,
        format!(
            "the state directory belongs to {} as {}; rebinding is an ownership migration and is not supported",
            owner.profile, owner.machine_id
        ),
    )
}

fn bind(ctx: &Ctx, profile: String, machine_id: String) -> Result<String, Refusal> {
    check_supported(&ctx.os, &ctx.arch)?;
    let invalid = |d: String| Refusal::new(Code::ProfileInvalid, d);
    let real =
        fs::canonicalize(&profile).map_err(|e| invalid(format!("{profile}: {}", e.kind())))?;
    if !real.is_file() {
        return Err(invalid(format!("{profile} is not a regular file")));
    }
    let real = real
        .to_str()
        .ok_or_else(|| invalid("profile path is not UTF-8".into()))?;
    let mut content = Vec::new();
    fs::File::open(real)
        .and_then(|f| f.take(MAX_BYTES as u64 + 1).read_to_end(&mut content))
        .map_err(|e| invalid(format!("{real}: {}", e.kind())))?;
    check_profile(&content)?;
    let want = Binding::new(real, &machine_id)?;
    let stamp = Stamp::of(&want);
    let (layout, sys) = (ctx.layout(), ctx.sys()?);

    // Phase 1 decides from a read-only look and refuses before any write. A different identity,
    // in the binding or in the state directory, is never replaced.
    let have = read_existing_binding(ctx)?;
    if let Some(h) = &have
        && !h.same_identity(&want)
    {
        return Err(rebind_refusal(h));
    }
    let owned = match Store::open_read(sys, &layout, Fail::none())? {
        Some(s) => {
            let doc = s.read_state()?;
            if !doc.binding.same_identity(&stamp) {
                return Err(state_owner_refusal(&doc.binding));
            }
            true
        }
        None => false,
    };
    if have.is_some() {
        return if owned {
            Ok("already bound".into())
        } else {
            Err(state_lost_refusal())
        };
    }

    // Phase 2 writes under the manager lock and decides again, because another command may have
    // bound in between. The state records the identity first, so a crash between the two writes
    // can only be retried by the same identity.
    // Phase 1 returned for an existing binding, so only an unbound machine gets here and may
    // create the state root.
    let mut store = Store::open_write_with(sys, &layout, state_cmd::fail_from_env()?, true)?;
    // Whatever this call created (directories, the lock file, the staging directory) makes a
    // later refusal a post-mutation one (exit 1). The store tracks that.
    let stamped = |store: &Store, r: Refusal| {
        if store.mutated() {
            r.after_mutation()
        } else {
            r
        }
    };
    let have = read_existing_binding(ctx).map_err(|r| stamped(&store, r))?;
    if let Some(h) = &have
        && !h.same_identity(&want)
    {
        return Err(stamped(&store, rebind_refusal(h)));
    }
    if have.is_some() && !store.is_initialized() {
        return Err(stamped(&store, state_lost_refusal()));
    }
    if store.is_initialized() {
        let doc = store.read_state().map_err(|r| stamped(&store, r))?;
        if !doc.binding.same_identity(&stamp) {
            return Err(stamped(&store, state_owner_refusal(&doc.binding)));
        }
    } else {
        store.init(&stamp)?;
    }
    let after = |r: Refusal| stamped(&store, r);
    if have.is_some() {
        return Ok("already bound".into());
    }
    let dir = open_config(&sys, &layout, true)
        .map_err(after)?
        .ok_or_else(|| {
            after(Refusal::new(
                Code::StorageInvalid,
                "the config directory vanished",
            ))
        })?;
    let name = ctx.binding_name().map_err(after)?;
    if publish(&dir, &name, &want).map_err(after)? {
        return Ok(format!(
            "bound {} as {machine_id}",
            dfm_model::display::visible(real)
        ));
    }
    match read_binding(&sys, &dir, &name).map_err(after)? {
        Some(have) if have.same_identity(&want) => Ok("already bound".into()),
        Some(have) => Err(after(rebind_refusal(&have))),
        None => Err(after(Refusal::new(
            Code::BindingInvalid,
            "binding vanished during bind",
        ))),
    }
}

/// Writes the binding with create-new semantics. `Ok(false)` means the file already existed.
/// The content is complete before it becomes visible: temporary file, fsync, then `linkat`
/// (which never replaces an existing file), all relative to the verified directory.
fn publish(dir: &Dir, name: &str, b: &Binding) -> Result<bool, Refusal> {
    dir.publish_new(name, b.to_toml().as_bytes(), &Fail::none(), "binding")
}

/// `self-check --fixture-dir`: reads only that directory and makes no network call.
fn fixture_check(dir: &Path) -> Result<(), Refusal> {
    let not_dir = |_| {
        Refusal::new(
            Code::BindingInvalid,
            format!("{} is not a directory", dir.display()),
        )
    };
    let d = Dir::open_plain(dir).map_err(not_dir)?;
    let uid = dfm_platform_macos::current_uid()
        .ok_or_else(|| Refusal::new(Code::StorageInvalid, "cannot read the user id"))?;
    read_binding(&Sys { uid }, &d, "machine.toml").map(|_| ())
}
