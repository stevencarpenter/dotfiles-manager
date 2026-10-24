//! `dfm self-update`: rebuilds and installs the manager from a local checkout by running the
//! target commit's own `scripts/bootstrap.sh install` (DFM-72, docs/bootstrap.md "Self-update").
//! The build, the controls, `self-check` and the `current` flip stay in the script; this command
//! picks the source and the commit. The script is the committed blob, read from the checkout's
//! objects, so for honest objects its identity check against the commit passes.

use crate::sync_cmd::Git;
use crate::{Ctx, flags, header, usage};
use dfm_engine::{Dir, Lock};
use dfm_model::display::visible;
use dfm_model::{Code, Refusal};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The checkout the last successful self-update built from, in the data directory.
const RECORD: &str = "source";
/// One self-update at a time: the script's build directory is shared, and so is the staging here.
const LOCK: &str = "self-update.lock";

pub(crate) struct Args {
    source: Option<String>,
    commit: Option<String>,
}

pub(crate) fn parse_args(rest: &[String]) -> Result<Args, Refusal> {
    let [source, commit] = flags(rest, ["--source", "--commit"])?;
    if source
        .as_deref()
        .is_some_and(|s| s.is_empty() || s.contains("://"))
    {
        return Err(usage(
            "--source must be the path of a local dotfiles-manager checkout",
        ));
    }
    if commit.as_deref().is_some_and(|c| !is_object_id(c)) {
        return Err(usage(
            "--commit must be a full lowercase hex object id (40 or 64 digits)",
        ));
    }
    Ok(Args { source, commit })
}

fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub(crate) fn run(ctx: &Ctx, args: &Args, out: &mut dyn Write) -> Result<(), Refusal> {
    let w =
        |e: std::io::Error| Refusal::new(Code::IoFailed, format!("output failed: {}", e.kind()));
    header(out, ctx).map_err(w)?;
    let data = ctx.platform.data_dir();
    let source = match &args.source {
        Some(s) => fs::canonicalize(s)
            .map_err(|e| usage(format!("--source {}: {}", visible(s), e.kind())))?,
        None => recorded(&data)?.ok_or_else(|| {
            usage(
                "self-update needs --source <dotfiles-manager checkout> once; later runs reuse it",
            )
        })?,
    };
    let src = source
        .to_str()
        .filter(|s| !s.chars().any(char::is_control))
        .ok_or_else(|| usage("--source must be a UTF-8 path without control characters"))?;
    let sys = ctx.sys()?;
    let git = Git::new(ctx, &sys);
    let rev = format!("{}^{{commit}}", args.commit.as_deref().unwrap_or("HEAD"));
    let commit = git.run(
        true,
        Code::SourceMissing,
        &format!("resolve {rev} in {}", visible(src)),
        &["-C", src, "rev-parse", "--verify", "--end-of-options", &rev],
    )?;
    if !is_object_id(&commit) || args.commit.as_ref().is_some_and(|c| *c != commit) {
        return Err(Refusal::new(
            Code::SourceMissing,
            format!("{} does not hold {rev} as a full object id", visible(src)),
        ));
    }
    let what = match args.commit {
        Some(_) => "the commit given",
        None => "the HEAD",
    };
    let shown = format!("{commit} ({what} of {})", visible(src));
    let cache = ctx.platform.cache_dir();
    fs::create_dir_all(&cache)
        .map_err(|e| dfm_engine::fsys::io(&cache.display().to_string(), &e))?;
    let _held = Lock::acquire(&sys, &Dir::open_plain(&cache)?, LOCK)?;
    if holds(&data, &commit) {
        writeln!(
            out,
            "self-update: {shown} is already installed; nothing to do"
        )
        .map_err(w)?;
        return record(&data, src);
    }
    let script = git.output(
        true,
        &[
            "-C",
            src,
            "cat-file",
            "blob",
            &format!("{commit}:scripts/bootstrap.sh"),
        ],
    )?;
    if !script.success() || script.stdout.is_empty() {
        return Err(Refusal::new(
            Code::SourceMissing,
            format!("commit {commit} has no scripts/bootstrap.sh"),
        ));
    }
    let dir = cache.join("self-update");
    let file = stage_script(&dir, &script.stdout)?;
    writeln!(out, "self-update: installing {shown}").map_err(w)?;
    out.flush().map_err(w)?;
    let status = bootstrap(ctx, &file, src, &commit);
    let _ = fs::remove_dir_all(&dir);
    let status = status.map_err(|e| {
        Refusal::new(
            Code::BootstrapFailed,
            format!("cannot start /bin/sh: {}", e.kind()),
        )
    })?;
    match (status.code(), status.signal()) {
        (Some(0), _) => record(&data, src),
        (Some(3), _) => Err(Refusal::new(
            Code::BootstrapFailed,
            "bootstrap.sh install refused before any change (exit 3, its error is above); nothing was installed",
        )),
        (Some(130), _) | (None, Some(2)) => Err(Refusal::new(
            Code::Interrupted,
            format!(
                "bootstrap.sh install was interrupted; {}",
                active(&data, &commit)
            ),
        )),
        (Some(143), _) | (None, Some(15)) => Err(Refusal::new(
            Code::Terminated,
            format!(
                "bootstrap.sh install was terminated; {}",
                active(&data, &commit)
            ),
        )),
        (code, signal) => Err(Refusal::new(
            Code::BootstrapFailed,
            format!(
                "bootstrap.sh install failed ({}, its error is above); {}",
                code.map_or_else(
                    || format!("signal {}", signal.unwrap_or(0)),
                    |c| format!("exit {c}")
                ),
                active(&data, &commit)
            ),
        )
        .after_mutation()),
    }
}

/// Which version is active after a run that did not finish: the script exits 1 also after the
/// flip (a read-back or a directory check that failed), so `current` is read again.
fn active(data: &Path, commit: &str) -> &'static str {
    if holds(data, commit) {
        "current already points at the new version; check it with dfm --version"
    } else {
        "the previous version stays active"
    }
}

/// Writes the script into a fresh private directory under the cache directory, outside the build
/// directory that `install` resets. Only the holder of [`LOCK`] gets here.
fn stage_script(dir: &Path, script: &[u8]) -> Result<PathBuf, Refusal> {
    let io = |e: std::io::Error| dfm_engine::fsys::io(&dir.display().to_string(), &e);
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).map_err(io)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io)?;
    let file = dir.join("bootstrap.sh");
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&file)
        .and_then(|mut f| f.write_all(script))
        .map_err(io)?;
    Ok(file)
}

/// Runs the script as docs/bootstrap.md says to start it: `/bin/sh <file>`, with `HOME` and a
/// system `PATH` only (the script scrubs and re-executes itself), its output on this terminal.
/// It runs outside the runner, with no timeout: a build the owner watches.
fn bootstrap(
    ctx: &Ctx,
    file: &Path,
    src: &str,
    commit: &str,
) -> std::io::Result<std::process::ExitStatus> {
    let home = ctx.platform.home();
    let mut cmd = Command::new("/bin/sh");
    cmd.arg(file)
        .args(["install", "--source", src, "--commit", commit])
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .current_dir(home)
        .stdin(Stdio::null());
    // The isolated build's home is not the password database's: the script's test mode, with
    // the test hooks the caller set (the script accepts only its own DFM_TEST_* names).
    #[cfg(feature = "test-isolation")]
    {
        cmd.env("DFM_BOOTSTRAP_TEST", "1")
            .env("DFM_TEST_HOME", home);
        cmd.envs(
            std::env::vars().filter(|(k, _)| k.starts_with("DFM_TEST_") && k != "DFM_TEST_HOME"),
        );
    }
    cmd.status()
}

/// Whether `current` points at an installed version of `commit` (`versions/<semver>-<first 12
/// of the commit>` holding `dfm`). A dangling `current` holds nothing.
fn holds(data: &Path, commit: &str) -> bool {
    let current = data.join("current");
    current.join("dfm").is_file()
        && fs::read_link(&current).ok().is_some_and(|t| {
            t.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(&format!("-{}", &commit[..12])))
        })
}

/// The recorded checkout, or `None` when no self-update has recorded one.
fn recorded(data: &Path) -> Result<Option<PathBuf>, Refusal> {
    let path = data.join(RECORD);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(dfm_engine::fsys::io(&path.display().to_string(), &e)),
    };
    let line = text.strip_suffix('\n').unwrap_or(&text);
    if !line.starts_with('/') || line.chars().any(char::is_control) {
        return Err(Refusal::new(
            Code::StorageInvalid,
            format!(
                "{} does not hold one absolute path; run dfm self-update --source <checkout>",
                path.display()
            ),
        ));
    }
    Ok(Some(PathBuf::from(line)))
}

/// Records the checkout for the next run: a temporary file renamed over the record.
fn record(data: &Path, src: &str) -> Result<(), Refusal> {
    let path = data.join(RECORD);
    let tmp = data.join(format!("{RECORD}.tmp-{}", std::process::id()));
    fs::write(&tmp, format!("{src}\n"))
        .and_then(|()| fs::rename(&tmp, &path))
        .map_err(|e| {
            let _ = fs::remove_file(&tmp);
            Refusal::new(
                Code::IoFailed,
                format!(
                    "the install is done, but the checkout was not recorded in {}: {e}; pass --source next time",
                    path.display()
                ),
            )
        })
}
