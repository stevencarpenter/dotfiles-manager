//! `dotfiles-manager` command line: version, help, explicit profile binding and self-check.

mod controls_fs;
mod state_cmd;

pub use controls_fs::{check_build_controls, toolchain_tree_digest};

use dfm_engine::fsys::{canonical_home, check_dir_chain};
use dfm_engine::{Fail, Found, Layout, Store, Sys};
use dfm_model::binding::{Binding, MAX_BYTES, check_profile};
use dfm_model::state::Stamp;
use dfm_model::{Code, Exit, Refusal};
use dfm_platform_macos::{Platform, check_supported, current_platform};
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const HELP: &str = "\
dotfiles-manager: bind this machine to one dotfiles profile.

usage:
  dotfiles-manager --version | version
  dotfiles-manager --help | help
  dotfiles-manager bind --profile <file> --machine-id <id>
  dotfiles-manager binding
  dotfiles-manager state check
  dotfiles-manager self-check --build-controls <staged tree>
  dotfiles-manager self-check --fixture-dir <dir>

bind records the profile and machine identity once. A second bind with a
different profile or machine is refused (E_REBIND_UNSUPPORTED). No command
infers a profile, and nothing applies configuration.

exit codes: 0 ok, 1 failed, 2 usage, 3 blocked before mutation, 4 declined,
5 pending, 130 interrupted, 143 terminated.
";

const MAX_ERROR_CHARS: usize = 512;

/// Control characters and escapes become visible, and the length is bounded.
fn bounded(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().take(MAX_ERROR_CHARS) {
        if c.is_control() {
            out.extend(c.escape_default())
        } else {
            out.push(c)
        }
    }
    out
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
        }
    }

    fn sys(&self) -> Result<Sys, Refusal> {
        let uid = dfm_platform_macos::current_uid()
            .ok_or_else(|| Refusal::new(Code::StorageInvalid, "cannot read the user id"))?;
        Ok(Sys {
            uid,
            o_nofollow: dfm_platform_macos::O_NOFOLLOW,
            o_nonblock: dfm_platform_macos::O_NONBLOCK,
            eloop: dfm_platform_macos::ELOOP,
        })
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
            r.code.exit()
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
                    b.profile(),
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

fn header(out: &mut dyn Write, ctx: &Ctx) -> std::io::Result<()> {
    match &ctx.isolated {
        Some(v) => writeln!(out, "isolated: true {v}"),
        None => Ok(()),
    }
}

/// Reads the whole bounded file. `None` when it does not exist. The file is opened without
/// following symlinks and must be a regular file owned by the current user with mode 0600.
fn read_binding(path: &Path) -> Result<Option<Binding>, Refusal> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let invalid =
        |d: String| Refusal::new(Code::BindingInvalid, format!("{}: {d}", path.display()));
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(dfm_platform_macos::O_NOFOLLOW | dfm_platform_macos::O_NONBLOCK)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        // ELOOP is what O_NOFOLLOW reports for a symlink.
        Err(e) if e.raw_os_error() == Some(dfm_platform_macos::ELOOP) => {
            return Err(invalid("is a symlink".into()));
        }
        Err(e) => return Err(invalid(e.kind().to_string())),
    };
    let meta = file.metadata().map_err(|e| invalid(e.kind().to_string()))?;
    if !meta.is_file() {
        return Err(invalid("not a regular file".into()));
    }
    binding_meta_problem(meta.uid(), meta.mode(), dfm_platform_macos::current_uid())
        .map_or(Ok(()), |p| Err(invalid(p)))?;
    let mut buf = Vec::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| invalid(e.kind().to_string()))?;
    Binding::parse(&buf)
        .map(Some)
        .map_err(|r| Refusal::new(r.code, format!("{}: {}", path.display(), r.detail)))
}

/// Why a binding file's owner or mode is unacceptable, if it is. Separate so the owner rule can
/// be tested without a file owned by another user.
fn binding_meta_problem(file_uid: u32, mode: u32, current_uid: Option<u32>) -> Option<String> {
    if current_uid != Some(file_uid) {
        return Some("not owned by the current user".into());
    }
    if mode & 0o777 != 0o600 {
        return Some(format!("mode is {:o}, expected 0600", mode & 0o777));
    }
    None
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

/// Validates the topology of the binding's directory, then reads the binding. A redirected,
/// mistyped or badly owned directory is `E_STORAGE_INVALID`. Nothing is created.
fn read_existing_binding(ctx: &Ctx) -> Result<Option<Binding>, Refusal> {
    match config_chain(ctx, false)? {
        Found::Absent => Ok(None),
        Found::Present => read_binding(&ctx.binding_path),
    }
}

/// The config directory is shared with other tools, so an existing one only has to be closed
/// to group and others. A directory this call creates is 0700.
fn config_chain(ctx: &Ctx, create: bool) -> Result<Found, Refusal> {
    let layout = ctx.layout();
    let sys = ctx.sys()?;
    let home = canonical_home(&sys, &layout.home)?;
    let rel = dfm_engine::fsys::below(&layout.home, &layout.config_dir)?;
    check_dir_chain(&sys, &home, &rel, rel.len(), create)
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
    if have.is_some() && owned {
        return Ok("already bound".into());
    }

    // Phase 2 writes under the manager lock and decides again, because another command may have
    // bound in between. The state records the identity first, so a crash between the two writes
    // can only be retried by the same identity.
    let store = Store::open_write(sys, &layout, state_cmd::fail_from_env()?)?;
    let have = read_existing_binding(ctx)?;
    if let Some(h) = &have
        && !h.same_identity(&want)
    {
        return Err(rebind_refusal(h));
    }
    if store.is_initialized()? {
        let doc = store.read_state()?;
        if !doc.binding.same_identity(&stamp) {
            return Err(state_owner_refusal(&doc.binding));
        }
    } else {
        store.init(&stamp)?;
    }
    if have.is_some() {
        return Ok("already bound".into());
    }
    config_chain(ctx, true)?;
    if publish(&ctx.binding_path, &want)? {
        return Ok(format!("bound {real} as {machine_id}"));
    }
    match read_binding(&ctx.binding_path)? {
        Some(have) if have.same_identity(&want) => Ok("already bound".into()),
        Some(have) => Err(rebind_refusal(&have)),
        None => Err(Refusal::new(
            Code::BindingInvalid,
            "binding vanished during bind",
        )),
    }
}

/// Writes the binding with create-new semantics. `Ok(false)` means the file already existed.
/// The content is complete before it becomes visible: temp file, fsync, then `hard_link`
/// (which never replaces an existing file). The directory was validated by the caller.
fn publish(path: &Path, b: &Binding) -> Result<bool, Refusal> {
    let io = |what: &str, e: std::io::Error| {
        Refusal::new(Code::IoFailed, format!("{what}: {}", e.kind()))
    };
    let parent = path
        .parent()
        .ok_or_else(|| Refusal::new(Code::BindingInvalid, "binding path has no parent"))?;
    // pid, a counter and the clock make the name unpredictable enough that create_new fails
    // rather than writing through a name an attacker prepared.
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let tmp = parent.join(format!(
        ".machine.toml.{}.{}.{nanos}.tmp",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| {
            // The umask can clear bits of the creation mode; the binding must be exactly 0600.
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
            f.write_all(b.to_toml().as_bytes())
                .and_then(|()| f.sync_all())
        });
    let linked = written.and_then(|()| fs::hard_link(&tmp, path));
    let _ = fs::remove_file(&tmp);
    match linked {
        Ok(()) => {
            // Make the new directory entry durable.
            fs::File::open(parent)
                .and_then(|d| d.sync_all())
                .map_err(|e| io("sync config dir", e))?;
            Ok(true)
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(io("write binding", e)),
    }
}

/// `self-check --fixture-dir`: reads only that directory and makes no network call.
fn fixture_check(dir: &Path) -> Result<(), Refusal> {
    if !fs::metadata(dir).is_ok_and(|m| m.is_dir()) {
        return Err(Refusal::new(
            Code::BindingInvalid,
            format!("{} is not a directory", dir.display()),
        ));
    }
    read_binding(&dir.join("machine.toml")).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::binding_meta_problem;

    #[test]
    fn owner_and_mode_rules() {
        assert_eq!(binding_meta_problem(501, 0o100600, Some(501)), None);
        assert!(
            binding_meta_problem(0, 0o100600, Some(501))
                .unwrap()
                .contains("owned")
        );
        assert!(
            binding_meta_problem(501, 0o100600, None)
                .unwrap()
                .contains("owned")
        );
        for mode in [0o100644, 0o100660, 0o100400, 0o100700] {
            assert!(
                binding_meta_problem(501, mode, Some(501))
                    .unwrap()
                    .contains("0600"),
                "{mode:o}"
            );
        }
    }
}
