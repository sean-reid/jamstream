use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    let task = env::args().nth(1);
    match task.as_deref() {
        Some("fixtures") => fixtures(),
        _ => {
            eprintln!("usage: cargo xtask <fixtures>");
            ExitCode::FAILURE
        }
    }
}

fn fixtures() -> ExitCode {
    eprintln!("fixture generation is not implemented yet");
    ExitCode::FAILURE
}
