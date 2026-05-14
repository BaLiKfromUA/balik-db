use std::path::Path;

use crate::error::Error;
use crate::storage::Storage;
use crate::storage::column_store::ColumnStore;

pub fn run(path: &Path) -> Result<(), Error> {
    let store = ColumnStore::open(path)?;
    let tables = store.list_tables()?;
    if tables.is_empty() {
        println!("(no tables)");
    } else {
        for name in tables {
            println!("{name}");
        }
    }
    Ok(())
}
