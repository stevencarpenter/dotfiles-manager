//! State persistence for the dotfiles manager: storage validation, the writer lock, atomic
//! writes, the journal, the ledger, receipts and trial records (docs/state.md).
//!
//! The crate names no OS constant: the caller supplies them in [`Sys`] and the paths in
//! [`Layout`].

pub mod apply;
pub mod fail;
pub mod fsys;
pub mod observe;
pub mod publish;
pub mod store;

pub use apply::{
    Generator, NoGenerator, RELOAD_NOT_ATTEMPTED, Report, Run, apply, owned_outputs, preflight,
};
#[cfg(feature = "failpoints")]
pub use fail::Action;
pub use fail::Fail;
pub use fsys::{Dir, Layout, Lock, Peek, PeekTo, Sys, listed, open_config, reset_listed};
pub use observe::{merge, observe};
pub use publish::Publish;
pub use store::{Finding, Loaded, Probe, Store, now_unix, verify_identity};
