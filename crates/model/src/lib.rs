//! Pure data and logic for the dotfiles manager. No filesystem, process or network access.

pub mod binding;
pub mod controls;
pub mod digest;
pub mod exit;
pub mod refusal;

pub use exit::Exit;
pub use refusal::{Code, Refusal};
