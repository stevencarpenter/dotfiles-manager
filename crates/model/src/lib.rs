//! Pure data and logic for the dotfiles manager. No filesystem, process or network access.

#![forbid(unsafe_code)]

pub mod adopt;
pub mod binding;
pub mod controls;
pub mod digest;
pub mod display;
pub mod doctor;
pub mod exit;
pub mod fold;
pub mod lock;
pub mod merge;
pub mod plan;
pub mod refusal;
pub mod resolve;
pub mod state;
pub mod updater;

pub use exit::Exit;
pub use fold::fold;
pub use refusal::{Code, Refusal};
