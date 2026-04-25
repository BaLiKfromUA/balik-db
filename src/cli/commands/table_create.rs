use std::path::Path;

use crate::catalog::schema::Schema;
use crate::catalog::tables::TableOptions;
use crate::error::Error;
use crate::storage::Storage;
use crate::storage::column_store::ColumnStore;

pub fn run(
    path: &Path,
    table: &str,
    columns: &str,
    row_group_size: Option<u32>,
) -> Result<(), Error> {
    let schema = Schema::parse_columns_dsl(columns)?;
    let mut store = ColumnStore::open(path)?;
    let opts = TableOptions { row_group_size };
    let id = store.create_table(table, schema, opts)?;
    println!("Created table '{table}' (id={id}) in '{}'", path.display());
    Ok(())
}
