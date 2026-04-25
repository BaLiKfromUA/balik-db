use std::path::Path;

use crate::error::Error;
use crate::storage::Storage;
use crate::storage::column_store::ColumnStore;

pub fn run(path: &Path, table: &str) -> Result<(), Error> {
    let store = ColumnStore::open(path)?;
    let desc = store.describe_table(table)?;
    println!("Table:          {}", desc.name);
    println!("ID:             {}", desc.id);
    println!("Storage:        {}", desc.storage_track);
    println!("Row group size: {}", desc.row_group_size);
    println!("Columns:");
    for col in &desc.schema.columns {
        let nullability = if col.nullable { "NULL" } else { "NOT NULL" };
        println!("  {:<24} {:<6} {}", col.name, col.ty.as_str(), nullability);
    }
    Ok(())
}
