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
    /// Parse a SQL query and print its AST. Does not touch storage; exits
    /// non-zero with a message on stderr if the query cannot be parsed.
    Parse {
        /// The SQL query to parse, as a single string.
        #[arg(long)]
        query: String,
    },

    /// Execute a SQL statement end to end and print its result. Runs the full
    /// pipeline (parse, plan, optimize, lower, execute) against storage; exits
    /// non-zero with a message on stderr on any error.
    Query {
        #[arg(long = "db", default_value = "./balik_db")]
        path: PathBuf,
        /// The SQL statement to execute, as a single string.
        #[arg(long)]
        sql: String,
    },

    /// Print the logical and physical plans for a SQL query without running it.
    /// Reads the catalog to validate references; exits non-zero on a parse or
    /// planning error.
    Explain {
        #[arg(long = "db", default_value = "./balik_db")]
        path: PathBuf,
        /// The SQL statement to plan, as a single string.
        #[arg(long)]
        sql: String,
        /// Apply logical optimizations before planning, matching what `query`
        /// executes.
        #[arg(long)]
        optimize: bool,
    },

    Doctor {
        #[arg(long = "db", default_value = "./balik_db")]
        path: PathBuf,
    },

    Init {
        #[arg(long = "db", default_value = "./balik_db")]
        path: PathBuf,
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

    /// Print every live row in a table, one per line.
    TableScan {
        #[arg(long = "db", default_value = "./balik_db")]
        path: PathBuf,
        #[arg(long)]
        table: String,
    },

    /// Delete a single row by its record id.
    RowDelete {
        #[arg(long = "db", default_value = "./balik_db")]
        path: PathBuf,
        #[arg(long)]
        table: String,
        /// Record id returned by a prior insert.
        #[arg(long)]
        rid: u64,
    },

    /// Update a single row by its record id. Prints the new rid since
    /// updates are modeled as delete + insert.
    RowUpdate {
        #[arg(long = "db", default_value = "./balik_db")]
        path: PathBuf,
        #[arg(long)]
        table: String,
        /// Record id returned by a prior insert.
        #[arg(long)]
        rid: u64,
        /// Comma-separated values matching the table's columns, e.g. "1,Alice".
        /// Use NULL (case-insensitive) for a NULL value.
        #[arg(long)]
        values: String,
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
