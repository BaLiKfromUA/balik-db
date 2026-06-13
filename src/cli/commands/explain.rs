use std::path::Path;

use crate::execution;
use crate::parser;
use crate::storage::column_store::ColumnStore;

/// Print the logical and physical plans for a SQL query without running it.
///
/// With `optimize`, logical optimizations (top-K fusion, column pushdown) run
/// before lowering, so the plans match what `query` would execute. The catalog
/// is read to validate table and column references; no rows are touched.
pub fn run(path: &Path, sql: &str, optimize: bool) -> Result<(), Box<dyn std::error::Error>> {
    let statement = parser::parse(sql)?;
    let store = ColumnStore::open(path)?;
    let mut logical = execution::plan(&statement, &store)?;
    if optimize {
        logical = execution::optimize(logical);
    }
    let physical = execution::lower(&logical);

    println!("Logical Plan:");
    println!("{logical}");
    println!();
    println!("Physical Plan:");
    println!("{physical}");
    Ok(())
}
