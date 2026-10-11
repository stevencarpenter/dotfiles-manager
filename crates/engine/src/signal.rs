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

const NONE: u8 = 0;
const INT: u8 = 1;
const TERM: u8 = 2;

/// The first signal received. Later signals do not change it, so the first one keeps its code.
static FIRST: AtomicU8 = AtomicU8::new(NONE);

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
/// taken and before the first mutation. A read-only command never calls it.
#[expect(
    unsafe_code,
    reason = "sigaction has no safe API; each block states its invariant"
)]
pub fn install() -> Result<(), Refusal> {
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
        // SAFETY: `act` is fully initialized and outlives the call. `on_signal` has the C ABI
        // and is async-signal-safe (see its comment). The old action is not wanted: null.
        if unsafe { libc::sigaction(sig, &act, std::ptr::null_mut()) } != 0 {
            return Err(failed("set"));
        }
    }
    Ok(())
}

/// The stop request, if a signal arrived since [`install`]. The refusal carries the code that
/// maps to exit 130 (SIGINT) or 143 (SIGTERM).
pub fn requested() -> Option<Refusal> {
    let (code, name) = match FIRST.load(Ordering::SeqCst) {
        INT => (Code::Interrupted, "SIGINT"),
        TERM => (Code::Terminated, "SIGTERM"),
        _ => return None,
    };
    Some(Refusal::new(
        code,
        format!("{name} received: no new operation was started"),
    ))
}
