use std::path::Path;

use crate::cli::values::render_record;
use crate::error::Error;
use crate::storage::Storage;
use crate::storage::column_store::ColumnStore;

pub fn run(path: &Path, table: &str) -> Result<(), Error> {
    let store = ColumnStore::open(path)?;
    let handle = store.open_table(table)?;
    let mut count = 0;
    for item in store.scan(&handle)? {
        let (rid, record) = item?;
        println!("rid {}: {}", rid.0, render_record(&handle.schema, &record));
        count += 1;
    }
    if count == 0 {
        println!("(no rows)");
    }
    Ok(())
}
