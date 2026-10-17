//! Bounded child processes for providers (docs/architecture.md section 8, "Read-only probe
//! allowlist", and section 10 item 6; docs/secrets.md).
//!
//! A provider never runs through a shell. [`Runner::run`] resolves the program on a fixed search
//! path (never the caller's `PATH`), checks the executable it found, builds the environment from
//! empty (`PATH`, `HOME`, `LANG=C` plus the variables the adapter names), starts the child in its
//! own process group, bounds what it reads from the child, and ends the child when the bound
//! elapses or a stop request arrives: SIGTERM to the group, a 10 s grace period, then SIGKILL.
//! The caller decides what the output means. Nothing here prints or persists a byte of it.

use crate::signal;
use dfm_model::{Code, Refusal};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Provider standard error is kept to this many bytes (section 8, redaction rule 3).
pub const MAX_STDERR: usize = 4096;
/// After SIGTERM the child gets this long before SIGKILL (section 10 item 6).
pub const GRACE: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(20);

/// Where providers are found and how their children are started.
#[derive(Clone, Debug)]
pub struct Runner {
    /// The directories searched for a program, in order. Never the caller's `PATH`.
    pub search_path: Vec<PathBuf>,
    /// `HOME` for the child. Providers keep their session and configuration under it.
    pub home: PathBuf,
    /// The child's working directory: a manager-owned directory, created with mode 0700.
    pub cwd: PathBuf,
    /// The current user. An executable owned by anyone but root or this user is refused.
    pub uid: u32,
}

/// One child to run.
pub struct Request<'a> {
    pub program: &'a str,
    pub args: &'a [String],
    /// Extra variables for the child, beside `PATH`, `HOME` and `LANG`.
    pub env: &'a [(String, String)],
    /// Bytes for the child's standard input. `None` closes it.
    pub stdin: Option<&'a [u8]>,
    pub timeout: Duration,
    /// More standard output than this is `E_LIMIT_EXCEEDED`.
    pub max_stdout: usize,
}

/// How the child ended.
#[derive(Debug)]
pub enum End {
    Exited(i32),
    /// Ended by a signal that was not ours.
    Signaled,
    /// The timeout elapsed: the group got SIGTERM, then SIGKILL.
    TimedOut,
    /// A stop request (SIGINT or SIGTERM to the manager) arrived while the child ran.
    Stopped(Refusal),
}

/// What the child left. `Debug` prints sizes, never bytes: a provider's output can be a secret.
pub struct Output {
    pub end: End,
    pub stdout: Vec<u8>,
    /// At most [`MAX_STDERR`] bytes. The caller redacts and bounds it again before it is shown.
    pub stderr: Vec<u8>,
}

impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Output")
            .field("end", &self.end)
            .field("stdout", &format!("{} bytes", self.stdout.len()))
            .field("stderr", &format!("{} bytes", self.stderr.len()))
            .finish()
    }
}

impl Output {
    pub fn success(&self) -> bool {
        matches!(self.end, End::Exited(0))
    }
}

fn refused(detail: impl Into<String>) -> Refusal {
    Refusal::new(Code::ProbeFailed, detail)
}

impl Runner {
    /// Finds `program` on the search path and checks the executable (section 8, argv[0] rule): a
    /// regular file with an execute bit, owned by root or the current user, not writable by group
    /// or others, whose resolved path stays under the parent of the search directory that named
    /// it (a package manager's link into its own prefix stays; a link out of the prefix does not).
    pub fn resolve(&self, program: &str) -> Result<PathBuf, Refusal> {
        if program.is_empty() || program.contains('/') || program.starts_with('-') {
            return Err(refused(format!("{program:?} is not a plain program name")));
        }
        for dir in &self.search_path {
            let candidate = dir.join(program);
            let Ok(meta) = std::fs::metadata(&candidate) else {
                continue;
            };
            let real = std::fs::canonicalize(&candidate)
                .map_err(|e| refused(format!("{}: {}", candidate.display(), e.kind())))?;
            let prefix = dir.parent().unwrap_or(dir);
            let shown = candidate.display();
            if !meta.is_file() {
                return Err(refused(format!("{shown} is not a regular file")));
            }
            if meta.mode() & 0o111 == 0 {
                return Err(refused(format!("{shown} is not executable")));
            }
            if meta.uid() != 0 && meta.uid() != self.uid {
                return Err(refused(format!(
                    "{shown} is owned by another user than root or you"
                )));
            }
            if meta.mode() & 0o022 != 0 {
                return Err(refused(format!("{shown} is writable by group or others")));
            }
            if !real.starts_with(prefix) {
                return Err(refused(format!(
                    "{shown} resolves to {}, outside {}",
                    real.display(),
                    prefix.display()
                )));
            }
            return Ok(candidate);
        }
        Err(refused(format!(
            "{program} was not found in {}",
            self.search_path
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(":")
        )))
    }

    fn joined_path(&self) -> String {
        self.search_path
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(":")
    }

    fn ensure_cwd(&self) -> Result<(), Refusal> {
        let e = |e: std::io::Error| refused(format!("{}: {}", self.cwd.display(), e.kind()));
        if !self.cwd.is_dir() {
            std::fs::create_dir_all(&self.cwd).map_err(e)?;
            std::fs::set_permissions(&self.cwd, std::fs::Permissions::from_mode(0o700))
                .map_err(e)?;
        }
        Ok(())
    }

    /// Runs one child to its end, or ends it. An error here means the child did not run (a
    /// program that was not found or failed its checks, a spawn failure). A child that ran and
    /// failed is an [`Output`] with its [`End`].
    pub fn run(&self, req: &Request<'_>) -> Result<Output, Refusal> {
        let program = self.resolve(req.program)?;
        self.ensure_cwd()?;
        if let Some(bad) = req.args.iter().find(|a| a.contains('\0')) {
            return Err(refused(format!("argument {bad:?} holds a NUL byte")));
        }
        let mut cmd = Command::new(&program);
        cmd.args(req.args)
            .env_clear()
            .env("PATH", self.joined_path())
            .env("HOME", &self.home)
            .env("LANG", "C")
            .envs(req.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .current_dir(&self.cwd)
            .stdin(if req.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| refused(format!("cannot start {}: {}", program.display(), e.kind())))?;
        let (out_rx, err_rx) = readers(&mut child, req.max_stdout);
        if let (Some(mut pipe), Some(bytes)) = (child.stdin.take(), req.stdin) {
            let bytes = bytes.to_vec();
            // A child that exits before it read everything closes the pipe; that is its business.
            std::thread::spawn(move || {
                let _ = pipe.write_all(&bytes);
            });
        }
        let end = wait(&mut child, req.timeout);
        let stdout = out_rx
            .recv()
            .map_err(|_| refused("the standard output reader ended early"))??;
        let stderr = err_rx.recv().unwrap_or_default();
        Ok(Output {
            end,
            stdout,
            stderr,
        })
    }
}

/// Reads both pipes on their own threads, so a child that fills one while we wait on the other
/// cannot stall. Standard output over the cap is `E_LIMIT_EXCEEDED`; standard error is cut.
#[expect(
    clippy::type_complexity,
    reason = "two receivers, named once at the one call site"
)]
fn readers(
    child: &mut Child,
    max_stdout: usize,
) -> (
    mpsc::Receiver<Result<Vec<u8>, Refusal>>,
    mpsc::Receiver<Vec<u8>>,
) {
    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    let mut stdout = child.stdout.take();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let r = match &mut stdout {
            Some(p) => p
                .take(
                    u64::try_from(max_stdout)
                        .unwrap_or(u64::MAX)
                        .saturating_add(1),
                )
                .read_to_end(&mut buf),
            None => Ok(0),
        };
        let r = match r {
            Err(e) => Err(Refusal::new(
                Code::IoFailed,
                format!("reading the provider's output: {}", e.kind()),
            )),
            Ok(_) if buf.len() > max_stdout => Err(Refusal::new(
                Code::LimitExceeded,
                format!("the provider wrote more than {max_stdout} bytes"),
            )),
            Ok(_) => Ok(buf),
        };
        let _ = out_tx.send(r);
    });
    let mut stderr = child.stderr.take();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = &mut stderr {
            // The rest is read and dropped so the child never blocks on a full pipe.
            let mut all = Vec::new();
            let _ = p.read_to_end(&mut all);
            all.truncate(MAX_STDERR);
            buf = all;
        }
        let _ = err_tx.send(buf);
    });
    (out_rx, err_rx)
}

/// Waits for the child, polling so a stop request or the timeout is seen within one poll.
fn wait(child: &mut Child, timeout: Duration) -> End {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return status.code().map_or(End::Signaled, End::Exited);
            }
            Ok(None) => {}
            Err(_) => return End::Signaled,
        }
        if let Some(stop) = signal::requested() {
            end_group(child);
            return End::Stopped(stop);
        }
        if started.elapsed() >= timeout {
            end_group(child);
            return End::TimedOut;
        }
        std::thread::sleep(POLL);
    }
}

/// SIGTERM to the child's group, the grace period, then SIGKILL, then reap.
fn end_group(child: &mut Child) {
    let pgid = child.id();
    signal::kill_group(pgid, true);
    let until = Instant::now() + GRACE;
    while Instant::now() < until {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(POLL);
    }
    signal::kill_group(pgid, false);
    let _ = child.wait();
}

/// Builds the extra environment from the caller's environment: only the variables whose name
/// starts with one of `prefixes` are carried over (for example a vault session token that is
/// already available). Nothing else of the caller's environment reaches a child.
pub fn passthrough(prefixes: &[&str]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .filter(|(k, _)| prefixes.iter().any(|p| k.starts_with(p)))
        .collect();
    out.sort();
    out
}

/// The directory under `cache_dir` a child runs in (`Platform::cache_dir()/run`).
pub fn run_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("run")
}
