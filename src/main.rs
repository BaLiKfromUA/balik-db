mod cli;

use std::process::ExitCode;
 
fn main() -> ExitCode {
    let args = cli::parse();
 
    match args.command {
        cli::Command::Doctor => println!("TODO: run doctor"),
        cli::Command::Init { path } => println!("TODO: init at '{}'", path.display()),
    }

    ExitCode::SUCCESS
    /*
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }*/
}
