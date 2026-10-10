//! Failure injection for tests: a named point where a command errors, aborts abruptly or waits.

use dfm_model::{Code, Refusal};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Return `E_IO_FAILED`, as a failing disk write would.
    Error,
    /// End the process at once with no cleanup, as a crash or SIGKILL would.
    Abort,
    /// Wait while holding whatever the caller holds, so a second command can collide.
    Sleep(Duration),
}

/// At most one armed point. [`Fail::none`] is inert, and release builds only ever pass that.
#[derive(Clone, Debug, Default)]
pub struct Fail(Option<(String, Action)>);

impl Fail {
    pub fn none() -> Fail {
        Fail(None)
    }

    pub fn at(point: &str, action: Action) -> Fail {
        Fail(Some((point.into(), action)))
    }

    pub fn hit(&self, point: &str) -> Result<(), Refusal> {
        match &self.0 {
            Some((p, a)) if p == point => match a {
                Action::Error => Err(Refusal::new(
                    Code::IoFailed,
                    format!("injected failure at {point}"),
                )),
                Action::Abort => std::process::abort(),
                Action::Sleep(d) => {
                    std::thread::sleep(*d);
                    Ok(())
                }
            },
            _ => Ok(()),
        }
    }
}
