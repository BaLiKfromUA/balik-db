use std::path::Path;

use crate::cli::values::parse_values;
use crate::error::Error;
use crate::storage::Storage;
use crate::storage::column_store::ColumnStore;

pub fn run(path: &Path, table: &str, values: &str) -> Result<(), Error> {
    let mut store = ColumnStore::open(path)?;
    let handle = store.open_table(table)?;
    let record = parse_values(&handle.schema, values)?;
    let rid = store.insert(&handle, record)?;
    println!("Inserted into '{table}' as rid {}", rid.0);
    Ok(())
}
