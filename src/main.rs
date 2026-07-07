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

    // Drive our own log level from `-v`/`-q`, but cap the noisy `sqlparser`
    // dependency — which emits a flood of debug/trace records while tokenizing —
    // at warn so it never drowns out the pipeline logs. The cap is also bounded
    // by the global level, so `-q` still silences everything.
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::filter::{LevelFilter, Targets};
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let level = args.verbose.tracing_level_filter();
    let filter = Targets::new()
        .with_default(level)
        .with_target("sqlparser", level.min(LevelFilter::WARN));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stdout)
        .with_ansi(std::io::stdout().is_terminal())
        .without_time()
        .with_target(false);

    tracing_subscriber::registry()
        .with(fmt_layer.with_filter(filter))
        .init();

    tracing::debug!(?args.command, "dispatching command");

    let result: Result<(), Box<dyn std::error::Error>> = match args.command {
        cli::Command::Parse { query } => cli::commands::parse::run(&query).map_err(Into::into),
        cli::Command::Query {
            path,
            sql,
            optimize,
        } => cli::commands::query::run(&path, &sql, optimize),
        cli::Command::Explain {
            path,
            sql,
            optimize,
        } => cli::commands::explain::run(&path, &sql, optimize),
        cli::Command::BenchGen {
            path,
            table,
            size,
            rows,
            seed,
        } => cli::commands::bench_gen::run(&path, &table, &size, rows, seed).map_err(Into::into),
        cli::Command::Doctor { path } => cli::commands::doctor::run(&path).map_err(Into::into),
        cli::Command::Init { path } => cli::commands::init::run(&path).map_err(Into::into),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
