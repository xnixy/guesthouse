//! `guesthouse-vm COMMAND [MACHINE]`: the machines from a terminal, with no window and no GTK, so it
//! runs from the release folder on a server. The work is in [`guesthouse_app_core::headless`].

use guesthouse_app_core::context::Context;
use guesthouse_app_core::headless::{Problem, run};
use std::env;
use std::io;
use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let context = match Context::from_process() {
        Ok(context) => context,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    match run(&context, &arguments, &mut io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(problem @ Problem::Usage(_)) => {
            eprintln!("{problem}");
            ExitCode::from(2)
        }
        Err(problem) => {
            eprintln!("{problem}");
            ExitCode::FAILURE
        }
    }
}
