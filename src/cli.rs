use clap::{Parser, Subcommand};
use std::path::PathBuf;
 
#[derive(Parser, Debug)]
#[command(name = "balik-cli", version, about, long_about = None)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}
 
#[derive(Subcommand, Debug)]
pub enum Command {
    Doctor,
 
    Init {
        #[arg(default_value = "balik_data")]
        path: PathBuf,
    },
}
 
pub fn parse() -> Args {
    Args::parse()
}
 

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use super::Args;

    #[test]
    fn verify_cli() {
	// based on https://docs.rs/clap/latest/clap/_tutorial/index.html#testing
        Args::command().debug_assert();
    }
}