use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exit = dfm_cli::run(&args, &mut std::io::stdout(), &mut std::io::stderr());
    let _ = std::io::stdout().flush();
    ExitCode::from(exit.code())
}
