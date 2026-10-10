//! State persistence for the dotfiles manager: storage validation, the writer lock, atomic
//! writes, the journal, the ledger, receipts and trial records (docs/state.md).
//!
//! The crate names no OS constant: the caller supplies them in [`Sys`] and the paths in
//! [`Layout`].

pub mod fail;
pub mod fsys;
pub mod store;

pub use fail::{Action, Fail};
pub use fsys::{Found, Layout, Lock, Sys};
pub use store::{Finding, Loaded, Probe, Store, now_unix, verify_identity};
