pub mod commands;

use clap::{Parser, Subcommand};
use clap_verbosity_flag::{Verbosity, WarnLevel};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "balik-cli", version, about, long_about = None)]
pub struct Args {
    #[command(flatten)]
    pub verbose: Verbosity<WarnLevel>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    Doctor {
        #[arg(long = "db", default_value = "./balik_db")]
        path: PathBuf,
    },

    Init {
        #[arg(long = "db", default_value = "./balik_db")]
        path: PathBuf,
    },
}

pub fn parse() -> Args {
    Args::parse()
}

#[cfg(test)]
mod tests {
    use super::Args;
    use clap::CommandFactory;

    #[test]
    fn verify_cli() {
        // based on https://docs.rs/clap/latest/clap/_tutorial/index.html#testing
        Args::command().debug_assert();
    }
}
