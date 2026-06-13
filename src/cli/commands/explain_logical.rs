use std::path::Path;

use crate::cli::OutputFormat;
use crate::execution::{self, LogicalPlan};
use crate::parser;
use crate::storage::column_store::ColumnStore;

/// Parse `query`, build its logical plan, and print it in `format`.
///
/// Bridges two error types — the parser's `ParseError` and the engine's
/// `Error` — so the return type is a boxed `std::error::Error`. `main` prints
/// it to stderr and exits non-zero. The catalog is read (to validate table and
/// column references) but no rows are touched.
pub fn run(
    path: &Path,
    query: &str,
    format: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let statement = parser::parse(query)?;
    let store = ColumnStore::open(path)?;
    let plan: LogicalPlan = execution::plan(&statement, &store)?;
    match format {
        OutputFormat::Tree => println!("{plan}"),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&plan)?),
    }
    Ok(())
}
