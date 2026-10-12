#![forbid(unsafe_code)]

use dfm_model::{Code, Exit};
use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    // `std::env::args()` panics on a non-UTF-8 argument. Refuse it as a usage error instead.
    let mut args = Vec::new();
    for (i, arg) in std::env::args_os().skip(1).enumerate() {
        match arg.into_string() {
            Ok(a) => args.push(a),
            Err(_) => {
                eprintln!(
                    "error: {} argument {} is not valid UTF-8",
                    Code::Usage.as_str(),
                    i + 1
                );
                return ExitCode::from(Exit::Usage.code());
            }
        }
    }
    let exit = dfm_cli::run(&args, &mut std::io::stdout(), &mut std::io::stderr());
    let _ = std::io::stdout().flush();
    ExitCode::from(exit.code())
}
