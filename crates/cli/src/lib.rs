//! `dotfiles-manager` command line: version, help, explicit profile binding and self-check.

mod controls_fs;

pub use controls_fs::{check_build_controls, toolchain_tree_digest};

use dfm_model::binding::{Binding, MAX_BYTES};
use dfm_model::{Code, Exit, Refusal};
use dfm_platform_macos::{Platform, check_supported, current_platform};
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const HELP: &str = "\
dotfiles-manager: bind this machine to one dotfiles profile.

usage:
  dotfiles-manager --version | version
  dotfiles-manager --help | help
  dotfiles-manager bind --profile <file> --machine-id <id>
  dotfiles-manager binding
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
    let w = |e: std::io::Error| {
        Refusal::new(Code::BindingInvalid, format!("output failed: {}", e.kind()))
    };
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
            match read_binding(&ctx.binding_path)? {
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

/// Reads the whole bounded file. `None` when it does not exist.
fn read_binding(path: &Path) -> Result<Option<Binding>, Refusal> {
    let invalid =
        |d: String| Refusal::new(Code::BindingInvalid, format!("{}: {d}", path.display()));
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(invalid(e.kind().to_string())),
        Ok(m) if !m.is_file() => return Err(invalid("not a regular file".into())),
        Ok(_) => {}
    }
    let mut buf = Vec::new();
    fs::File::open(path)
        .and_then(|f| f.take(MAX_BYTES as u64 + 1).read_to_end(&mut buf))
        .map_err(|e| invalid(e.kind().to_string()))?;
    Binding::parse(&buf)
        .map(Some)
        .map_err(|r| Refusal::new(r.code, format!("{}: {}", path.display(), r.detail)))
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
    let want = Binding::new(real, &machine_id)?;
    if let Some(have) = read_binding(&ctx.binding_path)? {
        return if have.same_identity(&want) {
            Ok("already bound".into())
        } else {
            Err(rebind_refusal(&have))
        };
    }
    if publish(&ctx.binding_path, &want)? {
        return Ok(format!("bound {real} as {machine_id}"));
    }
    // Another process bound first: same identity is success, anything else is a rebind.
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
/// (which never replaces an existing file).
fn publish(path: &Path, b: &Binding) -> Result<bool, Refusal> {
    let io = |what: &str, e: std::io::Error| {
        Refusal::new(Code::BindingInvalid, format!("{what}: {}", e.kind()))
    };
    let parent = path
        .parent()
        .ok_or_else(|| Refusal::new(Code::BindingInvalid, "binding path has no parent"))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|e| io("create config dir", e))?;
    let tmp = parent.join(format!(".machine.toml.{}.tmp", std::process::id()));
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(b.to_toml().as_bytes())
                .and_then(|()| f.sync_all())
        });
    let linked = written.and_then(|()| fs::hard_link(&tmp, path));
    let _ = fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(true),
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
