use std::path::Path;

use crate::cli::values::render_record;
use crate::error::Error;
use crate::storage::column_store::ColumnStore;
use crate::storage::{Rid, Storage};

pub fn run(path: &Path, table: &str, rid: u64) -> Result<(), Error> {
    let store = ColumnStore::open(path)?;
    let handle = store.open_table(table)?;
    match store.get(&handle, Rid(rid))? {
        Some(record) => println!("rid {rid}: {}", render_record(&handle.schema, &record)),
        None => println!("rid {rid}: not found"),
    }
    Ok(())
}
