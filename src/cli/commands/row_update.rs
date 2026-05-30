use std::path::Path;

use crate::cli::values::parse_values;
use crate::error::Error;
use crate::storage::column_store::ColumnStore;
use crate::storage::{Rid, Storage};

pub fn run(path: &Path, table: &str, rid: u64, values: &str) -> Result<(), Error> {
    let mut store = ColumnStore::open(path)?;
    let handle = store.open_table(table)?;
    let record = parse_values(&handle.schema, values)?;
    let new_rid = store.update(&handle, Rid(rid), record)?;
    println!("rid {rid}: updated as rid {}", new_rid.0);
    Ok(())
}
