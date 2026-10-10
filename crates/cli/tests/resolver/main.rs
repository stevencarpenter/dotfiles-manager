//! Executes the resolver against isolated, in-memory copies of the fixtures.

mod cases;
#[path = "../common/mod.rs"]
mod common;
mod harness;
mod limits;
mod native;
mod purity;
mod rules;
mod synthetic;
