//! The `--script` trial: fetch an install script over HTTPS and run it in a recorded window.

use super::{Args, ScriptArgs};
use crate::{Ctx, usage};
use dfm_model::Refusal;
use std::io::Write;

pub(super) fn run(
    _ctx: &Ctx,
    _args: &Args,
    _s: &ScriptArgs,
    _out: &mut dyn Write,
) -> Result<(), Refusal> {
    Err(usage("script trials are not implemented yet"))
}
