//! Bounded child processes for providers (docs/architecture.md section 8, "Read-only probe
//! allowlist", and section 10 item 6; docs/secrets.md).
//!
//! A provider never runs through a shell. [`Runner::run`] resolves the program on a fixed search
//! path (never the caller's `PATH`), checks the executable it found, builds the environment from
//! empty (`PATH`, `HOME`, `LANG=C` plus the variables the adapter names), starts the child in its
//! own process group once the manager's signal handlers are installed (before that it stays in
//! the manager's group, so a terminal SIGINT reaches it too), bounds what it reads from the
//! child, and ends the child when the bound elapses or a stop request arrives: SIGTERM, a grace
//! period, then SIGKILL. `run` returns within the timeout plus three grace periods whatever the
//! child and its descendants do: a process that keeps the inherited pipes open after the child
//! exited, or a child that closed them but ignores SIGTERM, gets the same SIGTERM, grace,
//! SIGKILL sequence. The caller decides what the output means. Nothing here prints or persists
//! a byte of it.

use crate::signal;
use dfm_model::{Code, Refusal};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Provider standard error is kept to this many bytes (section 8, redaction rule 3). The rest is
/// read in chunks and dropped, so memory stays at this bound.
pub const MAX_STDERR: usize = 4096;
/// After SIGTERM the child gets this long before SIGKILL (section 10 item 6).
pub const GRACE: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(20);
const CHUNK: usize = 8192;

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
    /// The grace period between SIGTERM and SIGKILL, [`GRACE`] in production. Tests shorten it.
    pub grace: Duration,
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
    /// or others, and (when the entry is a link) a link owned by root or the current user. The
    /// resolved path must stay under the search directory or under its parent prefix (a package
    /// manager's link into its own prefix stays; a link out of the prefix does not; for a
    /// directory directly under the root, such as `/bin`, the directory itself is the prefix).
    /// The first search directory that holds the name decides: a failed check there is refused,
    /// not shadowed by a later directory, and so is a directory where the name cannot be looked
    /// up (permission denied, an I/O error): only absence continues the search. The resolved
    /// path is what runs, so a link swapped after the check does not redirect the start.
    pub fn resolve(&self, program: &str) -> Result<PathBuf, Refusal> {
        if program.is_empty() || program.contains('/') || program.starts_with('-') {
            return Err(refused(format!("{program:?} is not a plain program name")));
        }
        for dir in &self.search_path {
            let candidate = dir.join(program);
            let shown = candidate.display();
            let entry = match std::fs::symlink_metadata(&candidate) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(refused(format!("{shown}: {}", e.kind()))),
            };
            let meta = std::fs::metadata(&candidate)
                .map_err(|e| refused(format!("{shown}: {}", e.kind())))?;
            let real = std::fs::canonicalize(&candidate)
                .map_err(|e| refused(format!("{shown}: {}", e.kind())))?;
            let prefix = match dir.parent() {
                Some(p) if p != Path::new("/") => p,
                _ => dir.as_path(),
            };
            if !meta.is_file() {
                return Err(refused(format!("{shown} is not a regular file")));
            }
            if meta.mode() & 0o111 == 0 {
                return Err(refused(format!("{shown} is not executable")));
            }
            for (m, what) in [(&meta, "is"), (&entry, "is a link that is")] {
                if m.uid() != 0 && m.uid() != self.uid {
                    return Err(refused(format!(
                        "{shown} {what} owned by another user than root or you"
                    )));
                }
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
            return Ok(real);
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

    /// The child's working directory: created with mode 0700 when missing; an existing one must
    /// be a real directory (not a link) owned by the current user, and is set to 0700 again.
    fn ensure_cwd(&self) -> Result<(), Refusal> {
        let e = |e: std::io::Error| refused(format!("{}: {}", self.cwd.display(), e.kind()));
        match std::fs::symlink_metadata(&self.cwd) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&self.cwd).map_err(e)?;
            }
            Err(err) => return Err(e(err)),
            Ok(m) if !m.is_dir() => {
                return Err(refused(format!(
                    "{} is not a plain directory",
                    self.cwd.display()
                )));
            }
            Ok(m) if m.uid() != self.uid => {
                return Err(refused(format!(
                    "{} is owned by another user",
                    self.cwd.display()
                )));
            }
            Ok(_) => {}
        }
        std::fs::set_permissions(&self.cwd, std::fs::Permissions::from_mode(0o700)).map_err(e)
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
            .stderr(Stdio::piped());
        // With the manager's handlers installed, the child gets its own group so the runner can
        // end it and everything it started. Before that (a credential gate, a plan-time probe), a
        // terminal SIGINT must reach the child too, so it stays in the manager's group and the
        // runner signals the child alone.
        let group = signal::installed();
        if group {
            cmd.process_group(0);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| refused(format!("cannot start {}: {}", program.display(), e.kind())))?;
        let target = Target {
            pid: child.id(),
            group,
        };
        let (out_rx, err_rx) = readers(&mut child, req.max_stdout);
        // The writer drops `fed_tx` when it is done. A process that keeps standard input open
        // without reading blocks it; until it is done the run is not settled, so that process
        // gets the termination sequence below, its pipe closes and the write fails.
        let (fed_tx, fed) = mpsc::channel::<()>();
        if let (Some(mut pipe), Some(bytes)) = (child.stdin.take(), req.stdin) {
            let bytes = bytes.to_vec();
            // A child that exits before it read everything closes the pipe; that is its business.
            std::thread::spawn(move || {
                let _ = pipe.write_all(&bytes);
                drop(fed_tx);
            });
        } else {
            drop(fed_tx);
        }
        let end = wait(&mut child, target, req.timeout);
        // Settled means the child is reaped, both output pipes are closed (which happens when
        // every process that inherited them is gone) and the standard input writer is done. A descendant that outlives the child, or a child
        // that closed its pipes but ignores SIGTERM, gets the same sequence as a timeout:
        // SIGTERM, grace, SIGKILL. The leader is unreaped until the end, so its id cannot have
        // been recycled.
        let mut pipes = Pipes::new(out_rx, err_rx);
        let ended_early = matches!(end, End::TimedOut | End::Stopped(_));
        if !ended_early && !settled(&mut child, &mut pipes, &fed, self.grace) {
            target.kill(true);
        }
        if !settled(&mut child, &mut pipes, &fed, self.grace) {
            target.kill(false);
        }
        if !settled(&mut child, &mut pipes, &fed, self.grace) {
            // Still there after SIGKILL: a process left the group, or the child cannot die. The
            // reader threads end when the pipes close; nothing more can be done from here.
            return Err(refused(format!(
                "a process started by {} is still running or keeps its output open after SIGKILL; it was left running",
                program.display()
            )));
        }
        let Pipes { stdout, stderr, .. } = pipes;
        let stdout = stdout
            .ok_or_else(|| refused("the standard output reader ended early"))?
            .or_else(|e| match end {
                // A stop request has precedence over a cap the child ran into at the same time.
                End::Stopped(_) => Ok(Vec::new()),
                _ => Err(e),
            })?;
        Ok(Output {
            end,
            stdout,
            stderr: stderr.unwrap_or_default(),
        })
    }
}

/// What a signal is sent to: the child's own process group, or the child alone when it shares
/// the manager's group.
#[derive(Clone, Copy)]
struct Target {
    pid: u32,
    group: bool,
}

impl Target {
    fn kill(self, term: bool) {
        signal::kill_child(self.pid, self.group, term);
    }
}

/// What the two reader threads have delivered so far.
struct Pipes {
    out_rx: mpsc::Receiver<Result<Vec<u8>, Refusal>>,
    err_rx: mpsc::Receiver<Vec<u8>>,
    stdout: Option<Result<Vec<u8>, Refusal>>,
    stderr: Option<Vec<u8>>,
}

impl Pipes {
    fn new(
        out_rx: mpsc::Receiver<Result<Vec<u8>, Refusal>>,
        err_rx: mpsc::Receiver<Vec<u8>>,
    ) -> Pipes {
        Pipes {
            out_rx,
            err_rx,
            stdout: None,
            stderr: None,
        }
    }

    fn done(&self) -> bool {
        self.stdout.is_some() && self.stderr.is_some()
    }

    /// Waits up to `limit` for both readers to end. A reader that panicked counts as ended.
    fn wait(&mut self, limit: Duration) -> bool {
        let until = Instant::now() + limit;
        while !self.done() {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            if self.stdout.is_none() {
                match self.out_rx.recv_timeout(left) {
                    Ok(v) => self.stdout = Some(v),
                    Err(mpsc::RecvTimeoutError::Timeout) => return false,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        self.stdout = Some(Err(refused("the standard output reader ended early")));
                    }
                }
            } else if self.stderr.is_none() {
                match self.err_rx.recv_timeout(left) {
                    Ok(v) => self.stderr = Some(v),
                    Err(mpsc::RecvTimeoutError::Timeout) => return false,
                    Err(mpsc::RecvTimeoutError::Disconnected) => self.stderr = Some(Vec::new()),
                }
            }
        }
        true
    }
}

/// Waits up to `limit` for the child to be reaped, both output pipes to close and the standard
/// input writer to finish. A `try_wait` error counts as reaped: nothing more can be learned
/// about the child.
fn settled(
    child: &mut Child,
    pipes: &mut Pipes,
    fed: &mpsc::Receiver<()>,
    limit: Duration,
) -> bool {
    let until = Instant::now() + limit;
    loop {
        let reaped = !matches!(child.try_wait(), Ok(None));
        let written = matches!(fed.try_recv(), Err(mpsc::TryRecvError::Disconnected));
        if reaped && written && pipes.done() {
            return true;
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        if pipes.done() {
            std::thread::sleep(left.min(POLL));
        } else {
            pipes.wait(left.min(POLL));
        }
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
        let mut kept = Vec::new();
        if let Some(p) = &mut stderr {
            // Read in chunks: the first MAX_STDERR bytes are kept, the rest is dropped as it
            // arrives, so the child never blocks on a full pipe and memory stays bounded.
            let mut chunk = [0u8; CHUNK];
            loop {
                match p.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let room = MAX_STDERR.saturating_sub(kept.len());
                        kept.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            }
        }
        let _ = err_tx.send(kept);
    });
    (out_rx, err_rx)
}

/// Waits for the child, polling so a stop request or the timeout is seen within one poll. On a
/// timeout or a stop request the target gets SIGTERM here; the caller waits the grace period for
/// the child to be reaped and its pipes to close, and sends SIGKILL otherwise. A `try_wait` error ends the target at
/// once, because nothing more can be learned about the child.
fn wait(child: &mut Child, target: Target, timeout: Duration) -> End {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return status.code().map_or(End::Signaled, End::Exited);
            }
            Ok(None) => {}
            Err(_) => {
                target.kill(false);
                return End::Signaled;
            }
        }
        if let Some(stop) = signal::requested() {
            target.kill(true);
            return End::Stopped(stop);
        }
        if started.elapsed() >= timeout {
            target.kill(true);
            return End::TimedOut;
        }
        std::thread::sleep(POLL);
    }
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
