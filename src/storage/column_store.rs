//! Column-store implementation of the `Storage` trait (Track B).
//!
//! Catalog-level methods delegate straight through to `Catalog`. The data
//! plane lays each table out as row groups of per-column `.col` files: a row
//! is appended by rewriting every column file in the open row group, and a
//! point read decodes the same offset out of each. `scan`, `update`, and
//! `delete` are not yet implemented.

use std::fs;
use std::path::{Path, PathBuf};

use crate::catalog::schema::{Column, ColumnType, Schema};
use crate::catalog::tables::{
    Catalog, DEFAULT_ROW_GROUP_SIZE, TableDescriptor, TableId, TableOptions,
};
use crate::error::Error;
use crate::storage::{
    Record, Rid, ScanIter, Storage, TableHandle, Value, column_file, delete_bitmap,
};

const ROW_GROUPS_DIR: &str = "row_groups";

/// Path of a row group directory within a table dir.
fn row_group_dir(table_dir: &Path, group_id: u32) -> PathBuf {
    table_dir
        .join(ROW_GROUPS_DIR)
        .join(format!("{group_id:06}"))
}

/// Create a row group directory with one empty `.col` file per column plus
/// the row group's `deletes.bm` bitmap, pre-sized for `row_group_size` rows.
fn materialize_row_group(
    rg_dir: &Path,
    columns: &[Column],
    row_group_size: u32,
) -> Result<(), Error> {
    fs::create_dir_all(rg_dir).map_err(|e| Error::io("create row group dir", e))?;
    for col in columns {
        let col_path = rg_dir.join(format!("{}.col", col.name));
        column_file::write_empty(&col_path, col.ty)?;
    }
    let bm_path = rg_dir.join(delete_bitmap::FILE_NAME);
    delete_bitmap::write_empty(&bm_path, row_group_size)?;
    Ok(())
}

/// Path of a column's `.col` file within a row-group directory.
fn col_path(rg_dir: &Path, col_name: &str) -> PathBuf {
    rg_dir.join(format!("{col_name}.col"))
}

/// Check a record's arity and per-value types against the schema before any
/// bytes are written, so a malformed row never reaches the column files.
fn validate_record(schema: &Schema, record: &Record) -> Result<(), Error> {
    if record.values.len() != schema.columns.len() {
        return Err(Error::invalid_value(format!(
            "record has {} value(s) but the table has {} column(s)",
            record.values.len(),
            schema.columns.len()
        )));
    }
    for (value, col) in record.values.iter().zip(&schema.columns) {
        match (value, col.ty) {
            (Value::Null, _) => {
                if !col.nullable {
                    return Err(Error::invalid_value(format!(
                        "column '{}' is NOT NULL but received NULL",
                        col.name
                    )));
                }
            }
            (Value::Int(_), ColumnType::Int) | (Value::Text(_), ColumnType::Text) => {}
            _ => {
                return Err(Error::invalid_value(format!(
                    "value for column '{}' does not match type {}",
                    col.name,
                    col.ty.as_str()
                )));
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct ColumnStore {
    catalog: Catalog,
}

impl ColumnStore {
    pub fn open(db_root: &Path) -> Result<Self, Error> {
        use crate::catalog::metadata::{Status, status};

        tracing::debug!(path = %db_root.display(), "opening column store");
        match status(db_root) {
            Status::Ok { .. } => {}
            Status::Missing => {
                return Err(Error::other(format!(
                    "'{}' is not an initialized balik database — run `init` first",
                    db_root.display()
                )));
            }
            Status::Unreadable => {
                return Err(Error::corrupt(format!(
                    "balik.meta at '{}' is corrupt or unreadable",
                    db_root.display()
                )));
            }
            Status::WrongMagic => {
                return Err(Error::other(format!(
                    "'{}' is not a balik database",
                    db_root.display()
                )));
            }
            Status::TooNew { found, supported } => {
                return Err(Error::corrupt(format!(
                    "database at '{}' uses format version {found}, this binary supports {supported}",
                    db_root.display()
                )));
            }
        }
        Ok(Self {
            catalog: Catalog::load(db_root)?,
        })
    }
}

impl Storage for ColumnStore {
    fn create_table(
        &mut self,
        name: &str,
        schema: Schema,
        options: TableOptions,
    ) -> Result<TableId, Error> {
        // Catalog owns table dir + manifest. Row-group layout is column-store
        // specific and lives below this layer — keeps catalog usable for any
        // future track.
        let columns = schema.columns.clone();
        let row_group_size = options.row_group_size.unwrap_or(DEFAULT_ROW_GROUP_SIZE);
        let id = self.catalog.create_table(name, schema, options)?;
        let table_dir = self.catalog.table_dir(name)?;
        let rg_dir = row_group_dir(&table_dir, 0);
        if let Err(e) = materialize_row_group(&rg_dir, &columns, row_group_size) {
            // Roll back the catalog publish so we don't leave a half-formed
            // table around. drop_table failure is logged inside the catalog.
            tracing::warn!(
                table = %name,
                error = %e,
                "failed to materialize initial row group, dropping table"
            );
            let _ = self.catalog.drop_table(name);
            return Err(e);
        }
        Ok(id)
    }

    fn open_table(&self, name: &str) -> Result<TableHandle, Error> {
        let desc = self.catalog.open_table(name)?;
        Ok(TableHandle {
            id: desc.id,
            name: desc.name,
            schema: desc.schema,
            dir: desc.dir,
            row_group_size: desc.row_group_size,
        })
    }

    fn list_tables(&self) -> Result<Vec<String>, Error> {
        Ok(self
            .catalog
            .list_tables()
            .into_iter()
            .map(String::from)
            .collect())
    }

    fn describe_table(&self, name: &str) -> Result<TableDescriptor, Error> {
        self.catalog.describe_table(name)
    }

    fn drop_table(&mut self, name: &str) -> Result<(), Error> {
        self.catalog.drop_table(name)
    }

    fn insert(&mut self, table: &TableHandle, record: Record) -> Result<Rid, Error> {
        validate_record(&table.schema, &record)?;

        let rgs = u64::from(table.row_group_size);
        let rid = self.catalog.describe_table(&table.name)?.next_rid;
        let group = (rid / rgs) as u32;
        let offset = rid % rgs;
        let rg_dir = row_group_dir(&table.dir, group);

        // Group 0 is materialized at create time; every later group opens when
        // its first row arrives.
        if offset == 0 && group > 0 {
            materialize_row_group(&rg_dir, &table.schema.columns, table.row_group_size)?;
        }

        // Append the value to each column file by rewriting the whole file.
        for (i, col) in table.schema.columns.iter().enumerate() {
            let path = col_path(&rg_dir, &col.name);
            let mut values = column_file::read_column(&path)?;
            values.push(record.values[i].clone());
            column_file::write_column(&path, col.ty, &values)?;
        }

        // Advance the allocator only after the row's data is durable, so a
        // crash mid-insert never hands out a rid for a half-written row.
        self.catalog.set_next_rid(&table.name, rid + 1)?;
        tracing::debug!(table = %table.name, rid, "inserted row");
        Ok(Rid(rid))
    }

    fn get(&self, table: &TableHandle, rid: Rid) -> Result<Option<Record>, Error> {
        let rgs = u64::from(table.row_group_size);
        let group = (rid.0 / rgs) as u32;
        let offset = (rid.0 % rgs) as usize;
        let rg_dir = row_group_dir(&table.dir, group);
        if !rg_dir.is_dir() {
            return Ok(None);
        }

        let mut values = Vec::with_capacity(table.schema.columns.len());
        for col in &table.schema.columns {
            let column = column_file::read_column(&col_path(&rg_dir, &col.name))?;
            match column.get(offset) {
                Some(value) => values.push(value.clone()),
                None => return Ok(None), // rid is past the last row in this group
            }
        }
        Ok(Some(Record { values }))
    }

    fn update(&mut self, _table: &TableHandle, _rid: Rid, _record: Record) -> Result<(), Error> {
        unimplemented!("update is not implemented yet")
    }

    fn delete(&mut self, _table: &TableHandle, _rid: Rid) -> Result<(), Error> {
        unimplemented!("delete is not implemented yet")
    }

    fn scan<'a>(&'a self, _table: &TableHandle) -> Result<ScanIter<'a>, Error> {
        unimplemented!("scan is not implemented yet")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::metadata;
    use crate::catalog::schema::{Column, ColumnType};
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Returns (TempDir, db_path) where db_path is an initialized balik db.
    fn init_db() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("db");
        metadata::initialize(&db_path).unwrap();
        (tmp, db_path)
    }

    fn schema_users() -> Schema {
        Schema {
            columns: vec![
                Column {
                    name: "id".to_string(),
                    ty: ColumnType::Int,
                    nullable: false,
                },
                Column {
                    name: "name".to_string(),
                    ty: ColumnType::Text,
                    nullable: true,
                },
            ],
        }
    }

    #[test]
    fn open_on_initialized_empty_db_returns_empty_store() {
        let (_tmp, db) = init_db();
        let store = ColumnStore::open(&db).unwrap();
        assert!(store.list_tables().unwrap().is_empty());
    }

    #[test]
    fn open_on_uninitialized_dir_fails() {
        let tmp = TempDir::new().unwrap();
        let err = ColumnStore::open(tmp.path()).unwrap_err();
        assert!(
            err.to_string()
                .contains("not an initialized balik database")
        );
    }

    #[test]
    fn open_on_nonexistent_path_fails() {
        let tmp = TempDir::new().unwrap();
        let err = ColumnStore::open(&tmp.path().join("nope")).unwrap_err();
        assert!(
            err.to_string()
                .contains("not an initialized balik database")
        );
    }

    #[test]
    fn open_on_wrong_magic_fails() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("db");
        std::fs::create_dir(&db).unwrap();
        std::fs::write(
            db.join("balik.meta"),
            crate::checksum::wrap(b"magic = \"sqlite\"\nformat_version = 1\ncreated = \"unix:0\"\n"),
        )
        .unwrap();
        let err = ColumnStore::open(&db).unwrap_err();
        assert!(err.to_string().contains("not a balik database"));
    }

    #[test]
    fn create_list_describe_roundtrip() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();

        assert_eq!(store.list_tables().unwrap(), vec!["users".to_string()]);

        let desc = store.describe_table("users").unwrap();
        assert_eq!(desc.name, "users");
        assert_eq!(desc.schema, schema_users());
    }

    #[test]
    fn create_table_materializes_row_group_zero() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();

        let rg0 = db
            .join("tables")
            .join("00000001")
            .join("row_groups")
            .join("000000");
        assert!(rg0.is_dir());

        // id.col -> INT, name.col -> TEXT, both empty headers.
        let id_h = column_file::read_header(&rg0.join("id.col")).unwrap();
        assert_eq!(id_h.logical_type, ColumnType::Int);
        assert_eq!(id_h.row_count, 0);
        assert_eq!(id_h.null_count, 0);
        assert!(!id_h.has_nulls());

        let name_h = column_file::read_header(&rg0.join("name.col")).unwrap();
        assert_eq!(name_h.logical_type, ColumnType::Text);
        assert_eq!(name_h.row_count, 0);

        let bm_path = rg0.join(delete_bitmap::FILE_NAME);
        assert!(bm_path.is_file(), "deletes.bm should be materialized");
        let bm_h = delete_bitmap::read_header(&bm_path).unwrap();
        assert_eq!(bm_h.deleted_count, 0);
    }

    #[test]
    fn open_table_returns_handle_with_schema() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();

        let handle = store.open_table("users").unwrap();
        assert_eq!(handle.name, "users");
        assert_eq!(handle.schema, schema_users());
        assert!(handle.dir.ends_with("tables/00000001"));
    }

    #[test]
    fn drop_table_works_through_trait() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        store.drop_table("users").unwrap();
        assert!(store.list_tables().unwrap().is_empty());
    }

    fn handle() -> TableHandle {
        TableHandle {
            id: 1,
            name: "users".to_string(),
            schema: schema_users(),
            dir: std::path::PathBuf::from("/tmp/unused"),
            row_group_size: 8192,
        }
    }

    fn empty_record() -> Record {
        Record { values: vec![] }
    }

    fn record(values: Vec<Value>) -> Record {
        Record { values }
    }

    #[test]
    fn insert_then_get_round_trips() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let h = store.open_table("users").unwrap();

        let r0 = store
            .insert(&h, record(vec![Value::Int(1), Value::Text("alice".to_string())]))
            .unwrap();
        let r1 = store
            .insert(&h, record(vec![Value::Int(2), Value::Null]))
            .unwrap();
        assert_eq!(r0, Rid(0));
        assert_eq!(r1, Rid(1));

        assert_eq!(
            store.get(&h, Rid(0)).unwrap(),
            Some(record(vec![Value::Int(1), Value::Text("alice".to_string())]))
        );
        assert_eq!(
            store.get(&h, Rid(1)).unwrap(),
            Some(record(vec![Value::Int(2), Value::Null]))
        );
    }

    #[test]
    fn get_unknown_rid_returns_none() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let h = store.open_table("users").unwrap();
        assert_eq!(store.get(&h, Rid(0)).unwrap(), None); // empty table
        store
            .insert(&h, record(vec![Value::Int(1), Value::Null]))
            .unwrap();
        assert_eq!(store.get(&h, Rid(99)).unwrap(), None); // past the end
    }

    #[test]
    fn insert_rejects_bad_records() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let h = store.open_table("users").unwrap();

        // wrong arity
        let err = store.insert(&h, record(vec![Value::Int(1)])).unwrap_err();
        assert!(err.to_string().contains("column(s)"));
        // wrong type for the id column
        let err = store
            .insert(&h, record(vec![Value::Text("x".to_string()), Value::Null]))
            .unwrap_err();
        assert!(err.to_string().contains("does not match"));
        // NULL into a NOT NULL column
        let err = store
            .insert(&h, record(vec![Value::Null, Value::Null]))
            .unwrap_err();
        assert!(err.to_string().contains("NOT NULL"));
    }

    #[test]
    fn insert_rolls_into_new_row_groups() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table(
                "t",
                schema_users(),
                TableOptions {
                    row_group_size: Some(2),
                },
            )
            .unwrap();
        let h = store.open_table("t").unwrap();
        for i in 0..5 {
            store
                .insert(&h, record(vec![Value::Int(i), Value::Text(format!("n{i}"))]))
                .unwrap();
        }

        // rgs=2: rids 0-1 in group 0, 2-3 in group 1, 4 opens group 2.
        let groups = db.join("tables").join("00000001").join("row_groups");
        assert!(groups.join("000001").is_dir());
        assert!(groups.join("000002").is_dir());

        for i in 0..5u64 {
            let rec = store.get(&h, Rid(i)).unwrap().unwrap();
            assert_eq!(rec.values[0], Value::Int(i as i64));
        }
    }

    #[test]
    fn rows_persist_across_reopen() {
        let (_tmp, db) = init_db();
        {
            let mut store = ColumnStore::open(&db).unwrap();
            store
                .create_table("users", schema_users(), TableOptions::default())
                .unwrap();
            let h = store.open_table("users").unwrap();
            store
                .insert(&h, record(vec![Value::Int(7), Value::Text("zed".to_string())]))
                .unwrap();
        }
        let store = ColumnStore::open(&db).unwrap();
        let h = store.open_table("users").unwrap();
        assert_eq!(
            store.get(&h, Rid(0)).unwrap(),
            Some(record(vec![Value::Int(7), Value::Text("zed".to_string())]))
        );
    }

    #[test]
    #[should_panic(expected = "not implemented yet")]
    fn update_is_unimplemented() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        let _ = store.update(&handle(), Rid(0), empty_record());
    }

    #[test]
    #[should_panic(expected = "not implemented yet")]
    fn delete_is_unimplemented() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        let _ = store.delete(&handle(), Rid(0));
    }

    #[test]
    #[should_panic(expected = "not implemented yet")]
    fn scan_is_unimplemented() {
        let (_tmp, db) = init_db();
        let store = ColumnStore::open(&db).unwrap();
        let _ = store.scan(&handle());
    }
}
