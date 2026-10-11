//! Stop requests from SIGINT and SIGTERM (docs/apply.md, "Signals").
//!
//! This is the only module of the workspace that contains `unsafe` code. A gate test
//! (`crates/cli/tests/gate.rs`) rejects the keyword in any other file. The handler does one
//! thing: it stores a number in a static atomic. The executor reads the number between
//! operations. Nothing here kills a process or exits from a handler.
#![allow(unsafe_code)]

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

/// Installs the handler for SIGINT and SIGTERM, without `SA_RESTART`, so a blocking call in the
/// main thread returns `EINTR` and the executor can look at the flag. Call it once, after the
/// writer lock is taken and before the first mutation. A read-only command never calls it.
pub fn install() -> Result<(), Refusal> {
    for sig in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: `libc::sigaction` is a plain C struct for which all-zero bytes are valid
        // (no flags, empty mask, null handler). Every field that matters is set below.
        let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
        act.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        act.sa_flags = 0;
        // SAFETY: `act.sa_mask` is a valid, exclusively borrowed `sigset_t`.
        let emptied = unsafe { libc::sigemptyset(&mut act.sa_mask) };
        // SAFETY: `act` is fully initialized and outlives the call. `on_signal` has the C ABI
        // and is async-signal-safe (see its comment). The old action is not wanted: null.
        let set = unsafe { libc::sigaction(sig, &act, std::ptr::null_mut()) };
        if emptied != 0 || set != 0 {
            return Err(Refusal::new(
                Code::IoFailed,
                format!(
                    "cannot install the signal handler: {}",
                    std::io::Error::last_os_error().kind()
                ),
            ));
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
