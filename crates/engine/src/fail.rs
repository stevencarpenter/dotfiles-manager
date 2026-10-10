//! Failure injection for tests: a named point where a command errors, aborts abruptly or waits.
//!
//! The armed variant exists only with the `failpoints` feature. Without it [`Fail`] is empty and
//! [`Fail::hit`] does nothing, so a release build contains no abort or wait code. A gate test
//! rejects a release dependency graph that enables the feature.

use dfm_model::Refusal;

#[cfg(feature = "failpoints")]
use std::path::PathBuf;

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
}

/// At most one armed point. [`Fail::none`] is inert.
#[derive(Clone, Debug, Default)]
pub struct Fail {
    #[cfg(feature = "failpoints")]
    armed: Option<(String, Action)>,
}

impl Fail {
    pub fn none() -> Fail {
        Fail::default()
    }

    #[cfg(feature = "failpoints")]
    pub fn at(point: &str, action: Action) -> Fail {
        Fail {
            armed: Some((point.into(), action)),
        }
    }

    #[cfg(feature = "failpoints")]
    pub fn hit(&self, point: &str) -> Result<(), Refusal> {
        use dfm_model::Code;
        match &self.armed {
            Some((p, a)) if p == point => match a {
                Action::Error => Err(Refusal::new(
                    Code::IoFailed,
                    format!("injected failure at {point}"),
                )),
                Action::Abort => std::process::abort(),
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
