//! Stop requests from SIGINT and SIGTERM (docs/apply.md, "Signals").
//!
//! This is the only module of the workspace with `unsafe` code: [`install`] and [`kill_child`]. A
//! gate test (`crates/cli/tests/gate.rs`) rejects the keyword and the lint attributes in any
//! other place. The handler does one thing: it stores a number in a static atomic. The executor
//! reads the number between operations, and once more inside an operation just before its
//! `intent` entry (see docs/apply.md, "Signals"). After that entry it does not read the number
//! until the operation ends. Nothing here kills a process or exits from a handler.
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
/// everything it started). A pid that does not fit, or a target that is gone, is ignored.
#[expect(
    unsafe_code,
    reason = "kill has no safe API; the call touches no memory"
)]
pub(crate) fn kill_child(pid: u32, group: bool, term: bool) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    let sig = if term { libc::SIGTERM } else { libc::SIGKILL };
    let target = if group { -pid } else { pid };
    // SAFETY: `kill` takes two plain integers and dereferences nothing. A negative pid names the
    // process group.
    let _ = unsafe { libc::kill(target, sig) };
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
