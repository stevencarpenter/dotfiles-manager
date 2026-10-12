//! Failure injection for tests: a named point where a command errors, aborts abruptly or waits.
//!
//! The armed variant exists only with the `failpoints` feature. Without it [`Fail`] is empty and
//! [`Fail::hit`] does nothing, so a release build contains no abort or wait code. A gate test
//! rejects a release dependency graph that enables the feature.

use dfm_model::Refusal;

#[cfg(feature = "failpoints")]
use std::path::PathBuf;
#[cfg(feature = "failpoints")]
use std::sync::Arc;
#[cfg(feature = "failpoints")]
use std::sync::atomic::{AtomicU32, Ordering};

#[cfg(feature = "failpoints")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Return `E_IO_FAILED`, as a failing disk write would.
    Error,
    /// End the process at once with no cleanup, as a crash or SIGKILL would.
    Abort,
    /// Block, holding whatever the caller holds, until this file exists (or 30 s pass). The
    /// point first creates `<file>.reached` (extension replaced), then the test creates the file.
    /// So the test waits on a file and never guesses a delay.
    Wait(PathBuf),
    /// Make this thread's next stop check see SIGTERM (`signal::simulate`), then go on. It lets a
    /// test deliver the stop request at one exact point of the run.
    Stop,
}

/// At most one armed point. [`Fail::none`] is inert.
#[derive(Clone, Debug, Default)]
pub struct Fail {
    /// The point, its action and the hit it fires on (0 means every hit).
    #[cfg(feature = "failpoints")]
    armed: Option<(String, Action, u32)>,
    /// How many times the armed point was reached. Shared by clones, so a store and a run that
    /// hold copies of one `Fail` count together.
    #[cfg(feature = "failpoints")]
    hits: Arc<AtomicU32>,
}

impl Fail {
    pub fn none() -> Fail {
        Fail::default()
    }

    /// Arms `point` for every hit.
    #[cfg(feature = "failpoints")]
    pub fn at(point: &str, action: Action) -> Fail {
        Fail::at_nth(point, action, 0)
    }

    /// Arms `point` for its `nth` hit only (counted from 1, so `at_nth(p, a, 2)` lets the first
    /// hit pass and fires on the second). 0 fires on every hit. A test uses it to let an intent
    /// entry through and fail the outcome entry that follows.
    #[cfg(feature = "failpoints")]
    pub fn at_nth(point: &str, action: Action, nth: u32) -> Fail {
        Fail {
            armed: Some((point.into(), action, nth)),
            hits: Arc::default(),
        }
    }

    #[cfg(feature = "failpoints")]
    pub fn hit(&self, point: &str) -> Result<(), Refusal> {
        use dfm_model::Code;
        match &self.armed {
            Some((p, _, nth))
                if p == point
                    && *nth != 0
                    && self.hits.fetch_add(1, Ordering::SeqCst) + 1 != *nth =>
            {
                Ok(())
            }
            Some((p, a, _)) if p == point => match a {
                Action::Error => Err(Refusal::new(
                    Code::IoFailed,
                    format!("injected failure at {point}"),
                )),
                Action::Abort => std::process::abort(),
                Action::Stop => {
                    crate::signal::simulate(true);
                    Ok(())
                }
                Action::Wait(file) => {
                    // Tell the test the point was reached, so it never has to guess a delay.
                    let _ = std::fs::write(file.with_extension("reached"), "");
                    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    while !file.exists() {
                        if std::time::Instant::now() > end {
                            return Err(Refusal::new(Code::IoFailed, "injected wait timed out"));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Ok(())
                }
            },
            _ => Ok(()),
        }
    }

    #[cfg(not(feature = "failpoints"))]
    pub fn hit(&self, _point: &str) -> Result<(), Refusal> {
        Ok(())
    }
}
