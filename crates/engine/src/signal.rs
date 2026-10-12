//! Stop requests from SIGINT and SIGTERM (docs/apply.md, "Signals").
//!
//! This is the only module of the workspace with `unsafe` code, and only [`install`] has it. A
//! gate test (`crates/cli/tests/gate.rs`) rejects the keyword and the lint attributes in any
//! other place. The handler does one thing: it stores a number in a static atomic. The executor
//! reads the number between operations. Nothing here kills a process or exits from a handler.
//!
//! The handlers are installed with `SA_RESTART`. The executor looks at the number only between
//! operations, and a restarted call keeps `openat`, `renameat`, `unlinkat` and `fsync` from
//! reporting a spurious `EINTR` as a failed or unknown operation.
//!
//! No child process runs after [`install`] today, so there is no 30 s wait and no process-group
//! escalation. Add them with the first operation that starts a child process.

use dfm_model::{Code, Refusal};
use std::sync::atomic::{AtomicU8, Ordering};

#[cfg(feature = "failpoints")]
use std::cell::Cell;
#[cfg(feature = "failpoints")]
use std::sync::atomic::AtomicBool;

const NONE: u8 = 0;
const INT: u8 = 1;
const TERM: u8 = 2;

/// The first signal received. Later signals do not change it, so the first one keeps its code.
static FIRST: AtomicU8 = AtomicU8::new(NONE);

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
    if result.is_err() {
        for (sig, old) in installed.iter().rev() {
            // SAFETY: `old` is the action `sigaction` itself returned for `sig`, so it is valid to
            // set again. The previous action is not wanted: null.
            unsafe { libc::sigaction(*sig, old, std::ptr::null_mut()) };
        }
    }
    result
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
