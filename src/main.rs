mod catalog;
mod cli;
mod commands;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args = cli::parse();

    let result: Result<(), Box<dyn std::error::Error>> = match args.command {
        cli::Command::Doctor => commands::doctor::run().map_err(Into::into),
        cli::Command::Init { path } => commands::init::run(&path).map_err(Into::into),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
