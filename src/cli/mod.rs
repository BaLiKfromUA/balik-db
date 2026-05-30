pub mod commands;
pub mod values;

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

    /// Create a new table.
    TableCreate {
        #[arg(long = "db", default_value = "./balik_db")]
        path: PathBuf,
        /// Table name.
        #[arg(long)]
        table: String,
        /// Comma-separated `name:TYPE` pairs, e.g. "id:INT,name:TEXT".
        /// Supported types: INT, TEXT.
        #[arg(long)]
        columns: String,
        /// Override the default row-group size for this table.
        #[arg(long = "row-group-size")]
        row_group_size: Option<u32>,
    },

    /// Insert a row into a table.
    RowInsert {
        #[arg(long = "db", default_value = "./balik_db")]
        path: PathBuf,
        /// Table name.
        #[arg(long)]
        table: String,
        /// Comma-separated values matching the table's columns, e.g. "1,Alice".
        /// Use NULL (case-insensitive) for a NULL value.
        #[arg(long)]
        values: String,
    },

    /// Read a single row by its record id.
    RowGet {
        #[arg(long = "db", default_value = "./balik_db")]
        path: PathBuf,
        /// Table name.
        #[arg(long)]
        table: String,
        /// Record id returned by a prior insert.
        #[arg(long)]
        rid: u64,
    },

    /// List all tables in the database.
    TableList {
        #[arg(long = "db", default_value = "./balik_db")]
        path: PathBuf,
    },

    /// Print the schema and layout info for one table.
    TableDescribe {
        #[arg(long = "db", default_value = "./balik_db")]
        path: PathBuf,
        #[arg(long)]
        table: String,
    },

    /// Drop a table and its on-disk files.
    TableDrop {
        #[arg(long = "db", default_value = "./balik_db")]
        path: PathBuf,
        #[arg(long)]
        table: String,
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
