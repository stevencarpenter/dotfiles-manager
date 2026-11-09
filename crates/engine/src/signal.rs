//! Stop requests from SIGINT and SIGTERM (docs/apply.md, "Signals").
//!
//! This is the only module of the workspace with `unsafe` code: the six functions that carry an
//! `expect` for the lint ([`install`], [`kill_child`], [`new_session`], [`foreground_group`],
//! [`take_terminal`] and [`terminal_is_ours`]). A gate test (`crates/cli/tests/gate.rs`) rejects
//! the keyword and the lint attributes in any other place. The handler does one thing: it stores
//! a number in a static atomic. The executor reads the number between operations, and once more
//! inside an operation just before its `intent` entry (see docs/apply.md, "Signals"). After that
//! entry it does not read the number until the operation ends. Nothing here kills a process or
//! exits from a handler.
//!
//! The handlers are installed with `SA_RESTART`. The executor never acts on the number in the
//! middle of a file system call, and a restarted call keeps `openat`, `renameat`, `unlinkat`
//! and `fsync` from reporting a spurious `EINTR` as a failed or unknown operation.
//!
//! A provider child (`crate::proc`) runs in its own process group once the handlers are
//! installed ([`installed`]). When a stop request or its timeout is seen while it runs,
//! [`kill_child`] sends SIGTERM to the group, the runner waits a grace period, and then it
//! sends SIGKILL (docs/secrets.md, "How `op` is run").

use dfm_model::{Code, Refusal};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

#[cfg(feature = "failpoints")]
use std::cell::Cell;

const NONE: u8 = 0;
const INT: u8 = 1;
const TERM: u8 = 2;

/// The first signal received. Later signals do not change it, so the first one keeps its code.
static FIRST: AtomicU8 = AtomicU8::new(NONE);
/// Whether [`install`] succeeded in this process. The runner puts a child in its own process
/// group only then: before, a terminal SIGINT must reach the child as it reaches the manager.
static INSTALLED: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "failpoints")]
thread_local! {
    /// A stop request made by a test on this thread only, so one test can end its own run
    /// without stopping the tests that run in parallel in the same process.
    static SIMULATED: Cell<u8> = const { Cell::new(NONE) };
}

/// Test seam: ends the next check of this thread as if SIGTERM (`term`) or SIGINT arrived.
#[cfg(feature = "failpoints")]
pub fn simulate(term: bool) {
    SIMULATED.with(|s| s.set(if term { TERM } else { INT }));
}

/// Test seam: makes [`install`] fail when it sets the SIGTERM action, after SIGINT is set.
#[cfg(feature = "failpoints")]
pub static FAIL_TERM_SET: AtomicBool = AtomicBool::new(false);

/// Test seam: makes [`install`] see a failed restore after an error. The actions are still put
/// back; only the result is replaced.
#[cfg(feature = "failpoints")]
pub static FAIL_RESTORE: AtomicBool = AtomicBool::new(false);

fn first() -> u8 {
    #[cfg(feature = "failpoints")]
    if let n @ (INT | TERM) = SIMULATED.with(Cell::get) {
        return n;
    }
    FIRST.load(Ordering::SeqCst)
}

/// The handler. Async-signal-safe: it calls only a lock-free atomic compare-and-swap. It does
/// not allocate, lock, print or touch errno.
extern "C" fn on_signal(sig: libc::c_int) {
    let code = if sig == libc::SIGINT { INT } else { TERM };
    let _ = FIRST.compare_exchange(NONE, code, Ordering::SeqCst, Ordering::SeqCst);
}

fn failed(what: &str) -> Refusal {
    Refusal::new(
        Code::IoFailed,
        format!(
            "cannot install the signal handler ({what}): {}",
            std::io::Error::last_os_error().kind()
        ),
    )
}

/// Installs the handler for SIGINT and SIGTERM. A signal that the parent set to ignore (for
/// example `nohup` or a background job) stays ignored. Call it once, after the writer lock is
/// taken and before the first mutation. A read-only command never calls it. On an error the
/// actions this call already changed are put back, so a failed call leaves no handler behind.
/// If a put-back itself fails, the error says so: a handler may still be installed.
#[expect(
    unsafe_code,
    reason = "sigaction has no safe API; each block states its invariant"
)]
pub fn install() -> Result<(), Refusal> {
    let mut installed: Vec<(libc::c_int, libc::sigaction)> = Vec::new();
    let result = (|| {
        for sig in [libc::SIGINT, libc::SIGTERM] {
            // SAFETY: `libc::sigaction` is a plain C struct for which all-zero bytes are valid
            // (no flags, empty mask, null handler).
            let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
            // SAFETY: a null `act` only reads the current action into `old`, a valid exclusive borrow.
            if unsafe { libc::sigaction(sig, std::ptr::null(), &mut old) } != 0 {
                return Err(failed("read"));
            }
            if old.sa_sigaction == libc::SIG_IGN {
                continue;
            }
            // SAFETY: as for `old`.
            let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
            act.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            act.sa_flags = libc::SA_RESTART;
            // SAFETY: `act.sa_mask` is a valid, exclusively borrowed `sigset_t`.
            if unsafe { libc::sigemptyset(&mut act.sa_mask) } != 0 {
                return Err(failed("mask"));
            }
            #[cfg(feature = "failpoints")]
            if sig == libc::SIGTERM && FAIL_TERM_SET.load(Ordering::SeqCst) {
                return Err(failed("set"));
            }
            // SAFETY: `act` is fully initialized and outlives the call. `on_signal` has the C ABI
            // and is async-signal-safe (see its comment). The old action is not wanted: null.
            if unsafe { libc::sigaction(sig, &act, std::ptr::null_mut()) } != 0 {
                return Err(failed("set"));
            }
            installed.push((sig, old));
        }
        Ok(())
    })();
    if result.is_ok() {
        INSTALLED.store(true, Ordering::SeqCst);
    }
    if result.is_err() {
        let mut restored = true;
        for (sig, old) in installed.iter().rev() {
            // SAFETY: `old` is the action `sigaction` itself returned for `sig`, so it is valid to
            // set again. The previous action is not wanted: null.
            let rc = unsafe { libc::sigaction(*sig, old, std::ptr::null_mut()) };
            restored &= rc == 0;
        }
        #[cfg(feature = "failpoints")]
        {
            restored &= !FAIL_RESTORE.load(Ordering::SeqCst);
        }
        if !restored {
            return Err(failed("restore: a handler may still be installed"));
        }
    }
    result
}

/// Whether the handlers are installed in this process.
pub fn installed() -> bool {
    INSTALLED.load(Ordering::SeqCst)
}

/// Sends SIGTERM (`term`) or SIGKILL to the child `pid`, or to its process group when the runner
/// gave it one (`group`; the group id is then the child's pid, so the signal reaches the child and
/// everything it started). SIGTERM is followed by SIGCONT: a stopped process (Ctrl-Z in a child
/// that has the terminal, or a self-sent SIGSTOP) that catches SIGTERM, as a script with a
/// cleanup `trap` does, keeps it pending until it runs again. A pid that does not fit, or a
/// target that is gone, is ignored.
#[expect(
    unsafe_code,
    reason = "kill has no safe API; the calls touch no memory"
)]
pub(crate) fn kill_child(pid: u32, group: bool, term: bool) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    let sig = if term { libc::SIGTERM } else { libc::SIGKILL };
    let target = if group { -pid } else { pid };
    // SAFETY: `kill` takes two plain integers and dereferences nothing. A negative pid names the
    // process group.
    unsafe {
        let _ = libc::kill(target, sig);
        if term {
            let _ = libc::kill(target, libc::SIGCONT);
        }
    }
}

/// Starts the child of `cmd` in a new session, with no controlling terminal: a program it runs
/// that opens `/dev/tty` (a `sudo` password prompt) fails at once instead of stopping in a
/// background group (docs/upgrade.md). The session leader's pid is its process group id, so
/// [`kill_child`] with `group` still reaches everything it started.
#[expect(
    unsafe_code,
    reason = "setsid has no safe API; the hook calls only that async-signal-safe function"
)]
pub(crate) fn new_session(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the hook runs in the child between fork and exec. It calls only `setsid`, which is
    // async-signal-safe, allocates nothing and touches no memory of the parent.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

/// Makes the child of `cmd` the foreground process group of the terminal on its standard input
/// before it starts, so its first read of the terminal does not stop it (`dfm trial --script`
/// with a terminal). The child leads its own group, so [`kill_child`] with `group` still reaches
/// everything it started. SIGTTOU is blocked around `tcsetpgrp`, which a background group may
/// otherwise not call. SIGTSTP is set to ignored: the manager does not wait for a stopped child,
/// so a Ctrl-Z would block the terminal until the timeout. An ignored disposition survives
/// `exec`, and a non-interactive shell does not reset it. A failure fails the spawn.
/// [`take_terminal`] gives the terminal back.
#[expect(
    unsafe_code,
    reason = "setpgid, tcsetpgrp, sigprocmask and signal have no safe API; the hook calls only async-signal-safe functions"
)]
pub(crate) fn foreground_group(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the hook runs in the child between fork and exec. It calls only `sigemptyset`,
    // `sigaddset`, `sigprocmask`, `setpgid`, `getpid`, `tcsetpgrp` and `signal`, which are
    // async-signal-safe, on two `sigset_t` values on its own stack (all-zero bytes are a valid
    // `sigset_t`). It allocates nothing and touches no memory of the parent. errno is read before
    // the mask is restored.
    unsafe {
        cmd.pre_exec(|| {
            let mut block: libc::sigset_t = std::mem::zeroed();
            let mut old: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut block);
            libc::sigaddset(&mut block, libc::SIGTTOU);
            if libc::sigprocmask(libc::SIG_BLOCK, &block, &mut old) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let result = if libc::setpgid(0, 0) != 0 || libc::tcsetpgrp(0, libc::getpid()) != 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            };
            libc::sigprocmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
            result?;
            if libc::signal(libc::SIGTSTP, libc::SIG_IGN) == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Makes the manager's process group the foreground group of the terminal on its standard input
/// again, after a child started with [`foreground_group`] ended. SIGTTOU is blocked on this
/// thread for the call: the manager is a background group until it succeeds. The thread's mask
/// is restored afterwards. A failed `tcsetpgrp` (for example, standard input is not a terminal)
/// is `E_IO_FAILED`.
#[expect(
    unsafe_code,
    reason = "pthread_sigmask and tcsetpgrp have no safe API; the calls touch only local sigset_t values"
)]
pub(crate) fn take_terminal() -> Result<(), Refusal> {
    // SAFETY: all-zero bytes are a valid `sigset_t`. Every pointer passed is to a local that
    // outlives the call, or null for an unwanted result. `pthread_sigmask` changes only this
    // thread's mask (`sigprocmask` is unspecified in a process with more than one thread), and
    // the old mask is set again before the block ends.
    let result = unsafe {
        let mut block: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut block);
        libc::sigaddset(&mut block, libc::SIGTTOU);
        let rc = libc::pthread_sigmask(libc::SIG_BLOCK, &block, &mut old);
        if rc != 0 {
            Err(std::io::Error::from_raw_os_error(rc))
        } else {
            let result = if libc::tcsetpgrp(0, libc::getpgrp()) != 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            };
            libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
            result
        }
    };
    result.map_err(|e| {
        Refusal::new(
            Code::IoFailed,
            format!("cannot take the terminal back from the child: {e}"),
        )
    })
}

/// Whether the manager's process group is the foreground group of the terminal on its standard
/// input. False when standard input is not a terminal.
#[expect(
    unsafe_code,
    reason = "tcgetpgrp and getpgrp have no safe API; the calls touch no memory"
)]
pub fn terminal_is_ours() -> bool {
    // SAFETY: both calls take or return plain integers and dereference nothing.
    unsafe { libc::tcgetpgrp(0) == libc::getpgrp() }
}

/// The stop request, if a signal arrived since [`install`]. The refusal carries the code that
/// maps to exit 130 (SIGINT) or 143 (SIGTERM).
pub fn requested() -> Option<Refusal> {
    let (code, name) = match first() {
        INT => (Code::Interrupted, "SIGINT"),
        TERM => (Code::Terminated, "SIGTERM"),
        _ => return None,
    };
    Some(Refusal::new(
        code,
        format!("{name} received: no new operation was started"),
    ))
}

#[cfg(all(test, feature = "failpoints"))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    const CHILD: &str = "DFM_SIGNAL_TEST_CHILD";
    const NAME: &str = "signal::tests::a_failed_install_puts_the_sigint_action_back";

    #[test]
    fn a_failed_restore_is_named_in_the_error() {
        FAIL_TERM_SET.store(true, Ordering::SeqCst);
        FAIL_RESTORE.store(true, Ordering::SeqCst);
        let e = install().unwrap_err();
        FAIL_TERM_SET.store(false, Ordering::SeqCst);
        FAIL_RESTORE.store(false, Ordering::SeqCst);
        assert!(e.detail.contains("restore"), "{e}");
        assert!(e.detail.contains("may still be installed"), "{e}");
    }

    #[test]
    fn a_failed_install_puts_the_sigint_action_back() {
        if std::env::var_os(CHILD).is_some() {
            // Child: SIGINT is set first, then the SIGTERM step fails.
            FAIL_TERM_SET.store(true, Ordering::SeqCst);
            assert!(install().is_err());
            let pid = std::process::id().to_string();
            std::process::Command::new("kill")
                .args(["-INT", &pid])
                .status()
                .unwrap();
            // With the default action restored the signal ends this process before the sleep
            // ends. A handler left behind records the signal and the process survives.
            std::thread::sleep(std::time::Duration::from_secs(3));
            std::process::exit(if first() == INT { 42 } else { 43 });
        }
        // A background job or `nohup` starts this process with SIGINT ignored, and `install`
        // then leaves SIGINT alone, so there would be nothing to put back. perl sets the default
        // action first and keeps it across the `exec`, so the check runs in every environment.
        let status = std::process::Command::new("perl")
            .args(["-e", "$SIG{INT} = 'DEFAULT'; exec { $ARGV[0] } @ARGV"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .status()
            .expect("perl is needed to reset an inherited SIGINT");
        assert_eq!(
            status.signal(),
            Some(libc::SIGINT),
            "the child survived SIGINT (exit {:?}): a handler was left installed",
            status.code()
        );
    }
}
