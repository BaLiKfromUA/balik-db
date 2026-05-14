use std::path::Path;

use crate::error::Error;
use crate::storage::Storage;
use crate::storage::column_store::ColumnStore;

pub fn run(path: &Path, table: &str) -> Result<(), Error> {
    let mut store = ColumnStore::open(path)?;
    store.drop_table(table)?;
    println!("Dropped table '{table}'");
    Ok(())
}
