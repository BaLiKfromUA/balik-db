use std::path::Path;

use crate::error::Error;
use crate::storage::column_store::ColumnStore;
use crate::storage::{Rid, Storage};

pub fn run(path: &Path, table: &str, rid: u64) -> Result<(), Error> {
    let mut store = ColumnStore::open(path)?;
    let handle = store.open_table(table)?;
    store.delete(&handle, Rid(rid))?;
    println!("rid {rid}: deleted");
    Ok(())
}
