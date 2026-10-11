//! State persistence for the dotfiles manager: storage validation, the writer lock, atomic
//! writes, the journal, the ledger, receipts and trial records (docs/state.md).
//!
//! The crate names no OS constant: the caller supplies them in [`Sys`] and the paths in
//! [`Layout`].
//!
//! `unsafe` is denied here. The one module that needs it is `signal`, which allows it for itself.

#![deny(unsafe_code)]

pub mod apply;
pub mod fail;
pub mod fsys;
pub mod generate;
pub mod observe;
pub mod publish;
pub mod signal;
pub mod store;

pub use apply::{
    Approval, Gates, RELOAD_NOT_ATTEMPTED, Report, Run, apply, check_allow, check_allowances,
    check_journal_capacity, flag, gates, owned_outputs,
};
#[cfg(feature = "failpoints")]
pub use fail::Action;
pub use fail::Fail;
pub use fsys::{
    Dir, Got, Layout, Live, Lock, Peek, PeekTo, Stamp, Sys, listed, open_config, reset_listed,
};
pub use generate::{Generator, GeneratorInputs, NoGenerator};
pub use observe::{merge, observe};
pub use store::{Finding, Loaded, Probe, Store, now_unix, verify_identity};
