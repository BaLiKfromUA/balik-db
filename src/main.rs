mod catalog;
mod checksum;
mod cli;
mod error;
mod execution;
mod fs_atomic;
mod parser;
mod storage;

use std::io::IsTerminal;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args = cli::parse();

    tracing_subscriber::fmt()
        .with_max_level(args.verbose.tracing_level_filter())
        .with_writer(std::io::stdout)
        .with_ansi(std::io::stdout().is_terminal())
        .without_time()
        .with_target(false)
        .init();

    tracing::debug!(?args.command, "dispatching command");

    let result: Result<(), Box<dyn std::error::Error>> = match args.command {
        cli::Command::Parse { query } => cli::commands::parse::run(&query).map_err(Into::into),
        cli::Command::Query { path, sql } => cli::commands::query::run(&path, &sql),
        cli::Command::Explain {
            path,
            sql,
            optimize,
        } => cli::commands::explain::run(&path, &sql, optimize),
        cli::Command::Doctor { path } => cli::commands::doctor::run(&path).map_err(Into::into),
        cli::Command::Init { path } => cli::commands::init::run(&path).map_err(Into::into),
        cli::Command::RowGet { path, table, rid } => {
            cli::commands::row_get::run(&path, &table, rid).map_err(Into::into)
        }
        cli::Command::TableList { path } => {
            cli::commands::table_list::run(&path).map_err(Into::into)
        }
        cli::Command::TableDescribe { path, table } => {
            cli::commands::table_describe::run(&path, &table).map_err(Into::into)
        }
        cli::Command::TableDrop { path, table } => {
            cli::commands::table_drop::run(&path, &table).map_err(Into::into)
        }
        cli::Command::RowDelete { path, table, rid } => {
            cli::commands::row_delete::run(&path, &table, rid).map_err(Into::into)
        }
        cli::Command::RowUpdate {
            path,
            table,
            rid,
            values,
        } => cli::commands::row_update::run(&path, &table, rid, &values).map_err(Into::into),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
