//! `dfm` command line (the dotfiles-manager binary): version, help, profile binding, state, explain, plan,
//! doctor, status, apply, trial and self-check.

#![forbid(unsafe_code)]

mod adapters;
mod apply_cmd;
mod controls_fs;
mod doctor_cmd;
mod paint;
mod plan_cmd;
mod read_fs;
mod self_update_cmd;
mod source_fs;
mod state_cmd;
mod sync_cmd;
mod trial_cmd;

pub use adapters::builtin_catalog;
pub use controls_fs::{check_build_controls, toolchain_tree_digest};

use dfm_engine::{Dir, Fail, Layout, Runner, Store, Sys, open_config};
use dfm_model::binding::{Binding, MAX_BYTES, check_profile};
use dfm_model::resolve::resolve;
use dfm_model::state::{Class, JournalEntry, Phase, Recovery, Stamp};
use dfm_model::{Code, Exit, Refusal};
use dfm_platform_macos::{Platform, check_supported, current_platform};
use source_fs::load_scoped;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const HELP: &str = "\
dfm: bind this machine to one dotfiles profile.

usage:
  dfm --version | version
  dfm --help | help
  dfm bind --profile <file> --machine-id <id> [--root <dir>]
  dfm binding
  dfm state check
  dfm state recover
  dfm state resolve <journal-id> --as done|failed
  dfm explain <resource-or-target> [--root <dir>]
  dfm plan [--root <dir>] [--json] [--against <saved plan>]
  dfm doctor [--root <dir>] [--json]
  dfm status [--root <dir>] [--json]
  dfm apply [--root <dir>] [--preview] [-y|--yes-plan <digest>] [--allow-disruptive <operation-id>]...
                               [--allow-adopt <operation-id>]...
  dfm secrets [--root <dir>] [--preview] [-y|--yes-plan <digest>] [--allow-adopt <operation-id>]...
  dfm packages [--root <dir>] [--preview] [-y|--yes-plan <digest>] [--update]
  dfm services [--root <dir>] [--preview] [-y|--yes-plan <digest>] [--allow-disruptive <operation-id>]...
                               [--restart <resource-id>]...
  dfm trial <package> [--id <trial-id>] [--provider homebrew/v1|mise/v1] [--config <path>]...
                               [--preview] [-y|--yes-plan <digest>]
  dfm sync [--root <dir>] [--preview] [-y|--yes-plan <digest>] [--allow-disruptive <operation-id>]...
                               [--allow-adopt <operation-id>]... [--restart <resource-id>]...
                               [--trust-origin <origin>]... [--no-upgrade]
  dfm self-update [--source <checkout>] [--commit <sha>]
  dfm self-check --build-controls <staged tree>
  dfm self-check --fixture-dir <dir>

bind records the profile and machine identity once, with the checkout root: --root, else the
nearest directory above the profile that holds .git. Every command that reads sources takes
that root when --root is not given; an explicit --root wins. A second bind with a different
profile or machine is refused (E_REBIND_UNSUPPORTED). On a binding from before the root was
recorded (schema 1), the same bind records the root in place; until then every command needs
--root. No command infers a profile. Only apply changes configuration files.

explain resolves the bound profile against the trusted sources in the root and prints
ownership, permitted changes, provenance and native precedence for one resource or
output path. It reads only the root and the binding, and writes nothing.

plan resolves the bound profile against the root, observes the claimed paths under HOME
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
digest of that exact plan. A plan that changes nothing needs no digest: it exits 0. Otherwise it
publishes file links and removes previously owned outputs, journals each step and writes a receipt.
A removal also needs --allow-disruptive <operation-id>. An existing link that is exactly the
planned link is adopted (ownership is recorded, nothing is written) only with --allow-adopt
<operation-id>.
--preview prints the plan and its digest and changes nothing.

secrets is apply over the secret resources only (op-template/v1): every other resource is listed as
not_in_scope. It needs a 1Password session for each account (`op whoami`), else it exits 3
(E_CREDENTIAL_UNAVAILABLE) before anything is written. With --yes-plan it renders each template
through `op inject` and publishes the result with the declared mode. A render that fails or is
empty keeps the previous file. An existing destination the ledger does not prove the manager
wrote needs --allow-adopt <operation-id>. No secret value, unsalted hash or provider message is
printed or stored; the ledger keeps a per-machine salted hash. See docs/secrets.md.

packages is apply over the package resources only (homebrew/v1, mise/v1, apt/v1, uv-tool/v1): every other
resource is listed as not_in_scope. Planning probes each provider read-only (Homebrew's Cellar
and Caskroom, mise ls, dpkg-query) with auto-update, analytics and mise configuration off, and
runs no tap code. A package the provider has installed is current; a missing official one is
installed with --yes-plan (brew install, mise install), with its missing dependencies named in
the plan. Nothing is upgraded, downgraded or removed. An executable of the same name from another owner, or a
provider that cannot answer, is a blocker (exit 3). apt is probe-only: installing needs root. See
docs/packages.md. With --update, each installed mise or uv tool declared rolling is upgraded to
the newest version its provider resolves (mise install, uv tool upgrade); installed versions
stay, a held tool or a local build is kept, and a copy from another owner blocks. See
docs/toolchains.md.

trial tries a tool without a module. It asks homebrew/v1 and mise/v1 which one lists or can
install the package (read-only probes, as packages runs them), records a baseline of the tool,
its owners and its configuration candidates (--config paths, or ~/.config/<tool>) before any
change, and prints the scope, the cleanup limits and a digest. With --yes-plan it writes the
trial record to state/trials/, installs the package when no provider has it (nothing when it
pre-exists), journals the install and writes a receipt. More than one capable provider needs
--provider. A candidate path is observed, never changed. adopt and purge are not built. See
docs/trial.md.

services is apply over the service resources only (launchd-agent/v1, brew-service/v1): every other
resource is listed as not_in_scope. Planning asks each launcher read-only (launchctl print, brew
services info) and observes the declared definition and configuration files, which must be
published first. A service the launcher holds as declared is current. One it does not hold is
started with --yes-plan (launchctl bootstrap, brew services start). A restart happens only on
--restart <resource-id>; it is disruptive and needs --allow-disruptive <operation-id> as a
removal does. A label loaded from a definition the manager does not own, or a daemon that is
loaded and not running, is a blocker (exit 3): the manager never unloads or replaces a process it
did not start without a request. A launcher that accepted a request without reporting completion
in time leaves the operation pending (exit 5), never updated. Nothing is stopped, unloaded for
good or uninstalled. See docs/lifecycle.md.

sync fetches the sources that dotfiles-manager.lock declares ([[sources]]: the root checkout tracks
its ref, a nested source is pinned to an exact commit) into a staging directory with an isolated git
environment, and plans the staged composition against this machine in four scopes: configuration,
packages (install what is missing), secrets and services (start what is not loaded). It prints the
four plans and one sync digest that covers them and the sources. sync is the one daily command and
the only one that runs without a digest: it then plans again under the lock and refuses
(E_PLAN_STALE) when that plan is not the printed one. -y <sync digest> is optional and pins the run
to a plan --preview printed. Every origin needs a grant in state/grants.json first (--trust-origin
<origin> writes one, prints the plan and stops; no fetch happens before the grant). The run
fast-forwards each checkout (never reset, clean, stash, rebase, commit or push; divergence and
overwritten local changes are refused), then runs apply, packages, secrets and services in that
order, each with its own plan digest and receipt. A removal of an owned output needs no flag. An
adoption or takeover runs only with --allow-adopt <operation-id>, else it is withheld and the rest
runs; a restart runs only with --restart <resource-id>. A scope that fails stops the later ones
(not_started). Without a 1Password session the secrets are skipped; a blocked packages, secrets or
services plan does not run; the other scopes run, and the last lines of the output warn what waits
and how to fix it. A blocked configuration plan refuses the run. A pin is never rewritten.
Then the updaters the profile declares (updaters = [...], from brew, claude, codex, opencode,
skills) run in order, detached, each reported updated, current, held, pending, failed or blocked,
with one upgrade receipt; a failure warns and the exit stays 0. --no-upgrade skips them.
--preview fetches into staging and changes nothing else. See docs/sync.md and docs/upgrade.md.

self-update rebuilds and installs the manager from a local dotfiles-manager checkout: it reads
scripts/bootstrap.sh of the commit (--commit, else the checkout's HEAD) and runs its install with
that commit, so the build and every check are the script's. --source names the checkout once;
later runs reuse it. A commit that current already points at is not built again. It never pulls:
run git pull in the checkout first. See docs/bootstrap.md.

An operation that ended unknown (E_OP_INTERRUPTED) blocks its path in every later run. Each apply
checks the path again and settles the entry when the result is clear. state recover does only that:
it needs no plan and no approval flag, takes the writer lock, journals what it settles, writes no
receipt, and exits 3 while an entry stays open. Doctor and status list open entries with their
journal id. state resolve <journal-id> --as done|failed settles one by hand: done needs the path
to hold the result the operation expected (else E_CONFLICT); failed needs nothing and is the only
way to clear an entry that has no intent record. Both journal the reason: resolved by operator.

exit codes: 0 ok, 1 failed, 2 usage, 3 blocked before mutation, 4 declined,
5 pending, 130 interrupted, 143 terminated.
";

const MAX_ERROR_CHARS: usize = 512;

/// The first `MAX_ERROR_CHARS` characters of `s`, not escaped. Use it where the sink escapes the
/// text itself (a doctor `Row`); escaping twice doubles each backslash.
fn clip(s: &str) -> String {
    s.chars().take(MAX_ERROR_CHARS).collect()
}

/// Control, invisible and bidirectional characters become visible escapes, and the length is
/// bounded.
fn bounded(s: &str) -> String {
    dfm_model::display::visible(&clip(s))
}

struct Ctx {
    platform: Platform,
    binding_path: PathBuf,
    os: String,
    arch: String,
    isolated: Option<String>,
    /// Color the text output: stdout is a terminal (`paint::enabled`).
    color: bool,
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

    /// How provider children (`op`) are started: the fixed search path of the platform (with
    /// `~/.local/bin` last), HOME, and a manager-owned working directory under the cache
    /// directory. The isolated build takes the search path from `DFM_PROVIDER_PATH` so tests can
    /// plant a provider look-alike.
    fn runner(&self, sys: &Sys) -> Runner {
        let search_path = provider_path(self.platform.home());
        Runner {
            search_path,
            home: self.platform.home().to_path_buf(),
            cwd: dfm_engine::proc::run_dir(&self.platform.cache_dir()),
            uid: sys.uid,
            grace: dfm_engine::proc::GRACE,
            detach: false,
        }
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

/// Spells HOME the way `Dir::open_home` and the store scope do (symlinks resolved), so every
/// path built from it (plan targets, ledger, recovery) lies inside the store's scope. A HOME that
/// cannot be resolved stays as given; opening it fails later with its own refusal. The resolved
/// spelling gets the same rule as the original: no whitespace, control characters or non-UTF-8
/// bytes (`E_HOME_MISMATCH`), because these paths reach shell scripts and tool arguments.
fn canonical_home(platform: Platform) -> Result<Platform, Refusal> {
    let Ok(real) = fs::canonicalize(platform.home()) else {
        return Ok(platform);
    };
    if !real.to_str().is_some_and(dfm_platform_macos::valid_home) {
        return Err(Refusal::new(
            Code::HomeMismatch,
            "the home directory resolves to a path with whitespace, control characters or non-UTF-8 bytes; this is not supported",
        ));
    }
    Ok(Platform::with_home(real))
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
    let platform = canonical_home(platform)?;
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
        color: paint::enabled(&std::io::stdout()),
    })
}

#[cfg(not(feature = "test-isolation"))]
fn context(_home: Option<String>) -> Result<Ctx, Refusal> {
    let platform = canonical_home(Platform::detect()?)?;
    let (os, arch) = current_platform();
    Ok(Ctx {
        binding_path: platform.binding_path(),
        platform,
        os: os.into(),
        arch: arch.into(),
        isolated: None,
        color: paint::enabled(&std::io::stdout()),
    })
}

#[cfg(feature = "test-isolation")]
fn provider_path(home: &Path) -> Vec<PathBuf> {
    match std::env::var("DFM_PROVIDER_PATH") {
        Ok(v) if !v.is_empty() => v.split(':').map(PathBuf::from).collect(),
        _ => dfm_platform_macos::provider_path(home),
    }
}

#[cfg(not(feature = "test-isolation"))]
fn provider_path(home: &Path) -> Vec<PathBuf> {
    dfm_platform_macos::provider_path(home)
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
            let tag = paint::style(paint::enabled(&std::io::stderr()), "1;31", "error:");
            let _ = writeln!(err, "{tag} {}", bounded(&r.to_string()));
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
                "dfm {} (commit {commit}){iso}",
                env!("CARGO_PKG_VERSION")
            )
            .map_err(w)
        }
        "bind" => {
            let [profile, id, root] = flags(rest, ["--profile", "--machine-id", "--root"])?;
            let ctx = context(home)?;
            header(out, &ctx).map_err(w)?;
            let msg = bind(
                &ctx,
                profile.ok_or_else(|| usage("--profile is required"))?,
                id.ok_or_else(|| usage("--machine-id is required"))?,
                root,
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
                    "bound profile={} machine_id={}{}",
                    dfm_model::display::visible(b.profile()),
                    b.machine_id(),
                    b.root()
                        .map(|r| format!(" root={}", dfm_model::display::visible(r)))
                        .unwrap_or_default()
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
            let text = explain(&ctx, &query, root.as_deref().map(Path::new))?;
            write!(out, "{text}").map_err(w)
        }
        "plan" => {
            let args = plan_cmd::parse_args(rest)?;
            plan_cmd::run(&context(home)?, &args, out)
        }
        "doctor" | "status" => {
            let name = if cmd == "doctor" { "doctor" } else { "status" };
            doctor_cmd::run(context(home), name, rest, out)
        }
        "apply" => {
            let args = apply_cmd::parse_args(rest, dfm_model::plan::Scope::Config)?;
            apply_cmd::run(&context(home)?, &args, out)
        }
        "secrets" => {
            let args = apply_cmd::parse_args(rest, dfm_model::plan::Scope::Secrets)?;
            if !args.allow.is_empty() {
                return Err(usage(
                    "secrets removes nothing; --allow-disruptive does not apply",
                ));
            }
            apply_cmd::run(&context(home)?, &args, out)
        }
        "packages" => {
            let args = apply_cmd::parse_args(rest, dfm_model::plan::Scope::Packages)?;
            if !args.allow.is_empty() || !args.allow_adopt.is_empty() {
                return Err(usage(
                    "packages removes and adopts nothing; --allow-disruptive and --allow-adopt do not apply",
                ));
            }
            apply_cmd::run(&context(home)?, &args, out)
        }
        "trial" => {
            let args = trial_cmd::parse_args(rest)?;
            trial_cmd::run(&context(home)?, &args, out)
        }
        "services" => {
            let args = apply_cmd::parse_args(rest, dfm_model::plan::Scope::Services)?;
            if !args.allow_adopt.is_empty() {
                return Err(usage(
                    "services adopts nothing; --allow-adopt does not apply",
                ));
            }
            apply_cmd::run(&context(home)?, &args, out)
        }
        "sync" => {
            let args = sync_cmd::parse_args(rest)?;
            sync_cmd::run(&context(home)?, &args, out)
        }
        "self-update" => {
            let args = self_update_cmd::parse_args(rest)?;
            self_update_cmd::run(&context(home)?, &args, out)
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
        "--help" | "-h" | "help" | "--version" | "-V" | "version" | "binding" => {
            Err(usage(format!("{cmd} takes no arguments")))
        }
        other => Err(usage(format!(
            "unknown command or argument {other}; try --help"
        ))),
    }
}

/// `explain <query> [--root <dir>]`, in either order.
fn explain_args(args: &[String]) -> Result<(String, Option<String>), Refusal> {
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
        root,
    ))
}

/// The bound profile resolved against the trusted sources in `root`.
struct Loaded {
    binding: Binding,
    /// The canonical `--root`: where the sources live (or will live) and how the plan names them.
    root: PathBuf,
    /// Where the sources were read from when that is not `root`: `sync` resolves a staged copy
    /// of the sources and plans it as the tree that `root` will hold after publication.
    staged: Option<PathBuf>,
    /// With `staged`: the absolute target of each planned configuration link, by its folded
    /// path (`dfm_model::fold`), as the file system compares paths. The
    /// services scope reads a definition at the target the configuration scope will publish.
    links: std::collections::BTreeMap<String, String>,
    /// With `staged`: the bytes of each file the configuration plan generates, by its folded
    /// path. The services scope reads a generated definition from these, not from HOME.
    generated: std::collections::BTreeMap<String, Vec<u8>>,
    sources: dfm_model::resolve::SourceSet,
    resolved: dfm_model::resolve::Resolved,
}

impl Loaded {
    /// The directory the source files are read from at plan time.
    fn read_root(&self) -> &Path {
        self.staged.as_deref().unwrap_or(&self.root)
    }
}

/// No `--root` and no root in the binding: a schema 1 binding (DFM-61).
fn no_root() -> Refusal {
    usage(
        "--root is required: the binding records no root. Pass --root <dir>, or record it once with dfm bind --root <dir> --profile <file> --machine-id <id> (the bound profile and machine id; --root may be left out when the checkout holds .git)",
    )
}

/// The canonical root (`--root`, else the root the binding records) and the bound profile's
/// path relative to it.
fn canonical_root(binding: &Binding, root: Option<&Path>) -> Result<(PathBuf, String), Refusal> {
    let root = root
        .or_else(|| binding.root().map(Path::new))
        .ok_or_else(no_root)?;
    let root = fs::canonicalize(root).map_err(|e| {
        Refusal::new(
            Code::ReportUnavailable,
            format!("{}: {}", root.display(), e.kind()),
        )
    })?;
    let rel = Path::new(binding.profile())
        .strip_prefix(&root)
        .ok()
        .and_then(Path::to_str)
        .ok_or_else(|| {
            Refusal::new(
                Code::ProfileInvalid,
                "the bound profile is not inside the root; point --root at the trusted source tree that contains it",
            )
        })?
        .to_string();
    Ok((root, rel))
}

/// The binding of this machine; an unbound machine is refused.
fn require_binding(ctx: &Ctx) -> Result<Binding, Refusal> {
    read_existing_binding(ctx)?.ok_or_else(|| {
        Refusal::new(
            Code::BindingInvalid,
            "this machine is not bound; run bind first",
        )
    })
}

/// Resolves the bound profile against `root` (default: the binding's root) without comparing it
/// to this machine. Read-only.
fn load_resolved_any_machine(ctx: &Ctx, root: Option<&Path>) -> Result<Loaded, Refusal> {
    let binding = require_binding(ctx)?;
    let (root, rel) = canonical_root(&binding, root)?;
    let sources = load_scoped(&root, &rel)?;
    let resolved = resolve(&sources, &rel, &builtin_catalog())?;
    Ok(Loaded {
        binding,
        root,
        staged: None,
        links: Default::default(),
        generated: Default::default(),
        sources,
        resolved,
    })
}

/// The architecture name a profile uses for this machine (`aarch64` is written `arm64`).
fn profile_arch(arch: &str) -> &str {
    if arch == "aarch64" { "arm64" } else { arch }
}

/// `Some(detail)` when the profile targets another platform or architecture than this machine.
fn machine_mismatch(ctx: &Ctx, loaded: &Loaded) -> Option<String> {
    target_mismatch(
        ctx,
        &loaded.resolved.platform,
        &loaded.resolved.architecture,
    )
}

/// As [`machine_mismatch`] for a declared `platform` and `arch`.
fn target_mismatch(ctx: &Ctx, platform: &str, arch: &str) -> Option<String> {
    (platform != ctx.os || arch != profile_arch(&ctx.arch)).then(|| {
        format!(
            "the profile targets {platform}/{arch} and this machine is {}/{}",
            ctx.os,
            profile_arch(&ctx.arch)
        )
    })
}

/// Resolves the bound profile against `root` and refuses a profile that targets another
/// platform or architecture than this machine (`E_PLATFORM_MISMATCH`). Read-only.
fn load_resolved(ctx: &Ctx, root: Option<&Path>) -> Result<Loaded, Refusal> {
    let loaded = load_resolved_any_machine(ctx, root)?;
    match machine_mismatch(ctx, &loaded) {
        Some(d) => Err(Refusal::new(Code::PlatformMismatch, d)),
        None => Ok(loaded),
    }
}

/// Resolves the bound profile against `root` and explains one resource. Read-only.
fn explain(ctx: &Ctx, query: &str, root: Option<&Path>) -> Result<String, Refusal> {
    // Explaining a profile for another machine is allowed; the mismatch is a note.
    let loaded = load_resolved_any_machine(ctx, root)?;
    let note = machine_mismatch(ctx, &loaded)
        .map(|d| format!("note: E_PLATFORM_MISMATCH {d}; apply and plan refuse it\n"))
        .unwrap_or_default();
    Ok(note + &dfm_model::resolve::explain(&loaded.resolved, query)?)
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

/// An explicit `bind --root` that differs from the root the binding records.
fn root_refusal(have: &str) -> Refusal {
    Refusal::new(
        Code::RebindUnsupported,
        format!(
            "already bound with root {}; changing the recorded root is not supported. Pass --root on each command instead",
            dfm_model::display::visible(have)
        ),
    )
}

/// A recorded root is kept: `bind --root` with another directory is refused.
fn keep_root(have: &Binding, flag: Option<&Path>) -> Result<(), Refusal> {
    match (have.root(), flag) {
        (Some(r), Some(f)) if f != Path::new(r) => Err(root_refusal(r)),
        _ => Ok(()),
    }
}

/// The nearest ancestor of `profile` that holds a `.git` entry (a directory, or a worktree's
/// file): the checkout `bind` records when no `--root` is given.
fn git_root(profile: &Path) -> Result<PathBuf, Refusal> {
    profile
        .ancestors()
        .skip(1)
        .find(|d| fs::symlink_metadata(d.join(".git")).is_ok())
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            usage(
                "no directory above the profile holds .git; pass --root <dir> to name the checkout that holds it",
            )
        })
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

/// Whether the ledger records a salted hash of a co-owned file. A ledger that cannot be read
/// proves nothing, so its error is returned and the caller creates no salt.
fn ledger_has_coowned(store: &Store) -> Result<bool, Refusal> {
    Ok(store
        .read_ledger_lenient()?
        .resources
        .iter()
        .any(|r| r.owned_outputs.iter().any(|o| o.coowned.is_some())))
}

fn bind(
    ctx: &Ctx,
    profile: String,
    machine_id: String,
    root: Option<String>,
) -> Result<String, Refusal> {
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
    let target = check_profile(&content)?;
    // Rebinding is unsupported, so a profile for another machine is refused before any write.
    // A key the profile lacks is left to the resolver, which reports it.
    let declared = |v: Option<&str>, own: &str| v.unwrap_or(own).to_string();
    if let Some(d) = target_mismatch(
        ctx,
        &declared(target.platform.as_deref(), &ctx.os),
        &declared(target.architecture.as_deref(), profile_arch(&ctx.arch)),
    ) {
        return Err(Refusal::new(Code::PlatformMismatch, d));
    }
    let want = Binding::new(real, &machine_id)?;
    // An explicit --root is checked as plan checks it, before anything else is decided.
    let flag = match root {
        Some(r) => {
            let (r, _) = canonical_root(&want, Some(Path::new(&r)))?;
            if !r.is_dir() {
                return Err(invalid(format!("{} is not a directory", r.display())));
            }
            Some(r)
        }
        None => None,
    };
    let (layout, sys) = (ctx.layout(), ctx.sys()?);

    // Phase 1 decides from a read-only look and refuses before any write. A different identity,
    // in the binding or in the state directory, is never replaced.
    let have = read_existing_binding(ctx)?;
    if let Some(h) = &have
        && !h.same_identity(&want)
    {
        return Err(rebind_refusal(h));
    }
    if let Some(h) = &have {
        keep_root(h, flag.as_deref())?;
    }
    let upgrade = have.as_ref().is_some_and(|h| h.root().is_none());
    let identity = Stamp::of(&want);
    let mut missing_salt = false;
    let owned = match Store::open_read(sys, &layout, Fail::none())? {
        Some(s) => {
            let doc = s.read_state()?;
            if !doc.binding.same_identity(&identity) {
                return Err(state_owner_refusal(&doc.binding));
            }
            // A store made before the salt was created at init gets it now (docs/state.md).
            missing_salt = s.salt()?.is_none();
            // A lost salt cannot be recreated when the ledger records salted hashes: they would
            // no longer match. That needs the recovery text, not a new salt.
            if missing_salt && ledger_has_coowned(&s)? {
                return Err(Refusal::new(
                    Code::StateInvalid,
                    dfm_model::merge::SALT_LOST,
                ));
            }
            true
        }
        None => false,
    };
    if have.is_some() && !(owned && (missing_salt || upgrade)) {
        return if owned {
            Ok("already bound".into())
        } else {
            Err(state_lost_refusal())
        };
    }
    // A new binding, or a schema 1 one upgraded in place, records --root, else the checkout
    // that holds the profile.
    let want = if have.is_none() || upgrade {
        let r = match &flag {
            Some(r) => r.clone(),
            None => git_root(Path::new(real))?,
        };
        let r = r
            .to_str()
            .ok_or_else(|| invalid("root path is not UTF-8".into()))?;
        want.with_root(r)?
    } else {
        want
    };
    let stamp = Stamp::of(&want);

    // Phase 2 writes under the manager lock and decides again, because another command may have
    // bound in between. The state records the identity first, so a crash between the two writes
    // can only be retried by the same identity.
    // Only a machine that phase 1 found unbound may create the state root; a salt repair or a
    // schema 1 upgrade needs the existing one.
    let fail = state_cmd::fail_from_env()?;
    let mut store = Store::open_write_with(sys, &layout, fail.clone(), have.is_none())?;
    // A test parks here, holding the lock, to damage the state between the two phases.
    fail.hit("bind-after-lock")?;
    // Whatever this call created (directories, the lock file, the staging directory) makes a
    // later refusal a post-mutation one (exit 1). The store tracks that.
    let have = read_existing_binding(ctx).map_err(|r| store.stamp(r))?;
    if let Some(h) = &have
        && !h.same_identity(&want)
    {
        return Err(store.stamp(rebind_refusal(h)));
    }
    if have.is_some() && !store.is_initialized() {
        return Err(store.stamp(state_lost_refusal()));
    }
    if store.is_initialized() {
        let doc = store.read_state().map_err(|r| store.stamp(r))?;
        if !doc.binding.same_identity(&stamp) {
            return Err(store.stamp(state_owner_refusal(&doc.binding)));
        }
    } else {
        store.init(&stamp)?;
    }
    let after = |r: Refusal| store.stamp(r);
    if store.salt().map_err(after)?.is_none() {
        if ledger_has_coowned(&store).map_err(after)? {
            return Err(after(Refusal::new(
                Code::StateInvalid,
                dfm_model::merge::SALT_LOST,
            )));
        }
        store.salt_or_create().map_err(after)?;
    }
    if let Some(h) = &have {
        keep_root(h, flag.as_deref()).map_err(after)?;
        return match h.root() {
            Some(_) => Ok("already bound".into()),
            None => upgrade_binding(ctx, &store, &want, &fail).map_err(after),
        };
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
            "bound {} as {machine_id} with root {}",
            dfm_model::display::visible(real),
            dfm_model::display::visible(want.root().unwrap_or_default())
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

/// Rewrites a schema 1 binding as `want` (schema 2, same identity) under the writer lock,
/// between an `upgrade` intent and its outcome, as `grant-origin` is journaled. The replacement
/// is atomic: the old binding or the new one, never part. A run that dies in between leaves the
/// intent open, which `doctor` and `state recover` report.
fn upgrade_binding(
    ctx: &Ctx,
    store: &Store,
    want: &Binding,
    fail: &Fail,
) -> Result<String, Refusal> {
    // Phase 1 saw a binding with a root, and phase 2 a schema 1 one: it changed in between.
    let root = want.root().ok_or_else(|| {
        Refusal::new(
            Code::Conflict,
            "the binding changed during bind; run bind again",
        )
    })?;
    let sys = ctx.sys()?;
    let dir = open_config(&sys, &ctx.layout(), false)?
        .ok_or_else(|| Refusal::new(Code::StorageInvalid, "the config directory vanished"))?;
    let name = ctx.binding_name()?;
    let at = dfm_engine::now_ns()?;
    let what = format!("binding schema 1 rewritten as schema 2 with root {root}");
    let entry = |phase: Phase, reason: String| JournalEntry {
        schema: dfm_model::state::SCHEMA,
        op_id: format!("binding-upgrade-{at}"),
        op: "upgrade".into(),
        phase,
        at: dfm_model::state::rfc3339(at),
        resource: "binding".into(),
        adapter: "manager".into(),
        class: Class::Normal,
        recovery: (phase == Phase::Intent).then(|| Recovery {
            outputs: Vec::new(),
            snapshot: None,
        }),
        token: None,
        reason: Some(reason),
    };
    store.append_journal(&entry(Phase::Intent, what.clone()))?;
    let wrote = dir.replace_file(&name, want.to_toml().as_bytes(), fail, "binding");
    // Before the rename the schema 1 binding stays; after it the new one is in place, even
    // when its durability is unconfirmed.
    let phase = match &wrote {
        Err(e) if e.code != Code::IoUnconfirmed => Phase::Failed,
        _ => Phase::Done,
    };
    let reason = wrote.as_ref().err().map_or(what, |e| clip(&e.detail));
    store.append_journal(&entry(phase, reason))?;
    wrote?;
    Ok(format!(
        "recorded root {} in the binding",
        dfm_model::display::visible(root)
    ))
}

/// Writes the binding with create-new semantics. `Ok(false)` means the file already existed.
/// The content is complete before it becomes visible: temporary file, fsync, then an exclusive
/// rename (which never replaces an existing file), all relative to the verified directory.
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
