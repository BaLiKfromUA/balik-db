use std::path::Path;

use crate::execution;
use crate::parser;
use crate::storage::column_store::ColumnStore;

/// Execute a SQL statement end to end and print its result.
///
/// Runs the full pipeline: parse → bind → optimize → lower → execute. A SELECT
/// prints its rows as a table; CREATE TABLE / INSERT print an effect line.
///
/// Bridges the parser's `ParseError` and the engine's `Error` behind a boxed
/// `std::error::Error`; `main` prints it to stderr and exits non-zero.
pub fn run(path: &Path, sql: &str) -> Result<(), Box<dyn std::error::Error>> {
    let statement = parser::parse(sql)?;
    let mut store = ColumnStore::open(path)?;
    let logical = execution::optimize(execution::plan(&statement, &store)?);
    let physical: execution::PhysicalPlan = execution::lower(&logical);
    let result: execution::QueryResult = execution::execute(&physical, &mut store)?;
    println!("{result}");
    Ok(())
}
