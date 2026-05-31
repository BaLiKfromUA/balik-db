//! Column-store implementation of the `Storage` trait (Track B).
//!
//! Catalog-level methods delegate straight through to `Catalog`. The data
//! plane lays each table out as row groups of per-column `.col` files: a row
//! is appended by rewriting every column file in the open row group, a point
//! read decodes the same offset out of each, `scan` walks groups in order
//! yielding live (`deletes.bm`-filtered) rows, `delete` flips the tombstone
//! bit so `get`/`scan` skip the row, and `update` is modeled as
//! `delete + insert` which reassigns the row's rid.

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

        // Load the row group's delete bitmap once so each column rewrite
        // can exclude tombstoned offsets from its min/max stats. The bitmap
        // never changes during an insert (deletes are a separate API call).
        let (_, deletes) = delete_bitmap::load(&rg_dir.join(delete_bitmap::FILE_NAME))?;

        // Append the value to each column file by rewriting the whole file.
        for (i, col) in table.schema.columns.iter().enumerate() {
            let path = col_path(&rg_dir, &col.name);
            let (_, mut values) = column_file::read_column(&path)?;
            values.push(record.values[i].clone());
            column_file::write_column(&path, col.ty, &values, &deletes)?;
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

        let (_, bitmap) = delete_bitmap::load(&rg_dir.join(delete_bitmap::FILE_NAME))?;
        if delete_bitmap::is_deleted(&bitmap, offset) {
            return Ok(None);
        }

        let mut values = Vec::with_capacity(table.schema.columns.len());
        for col in &table.schema.columns {
            let (_, column) = column_file::read_column(&col_path(&rg_dir, &col.name))?;
            match column.get(offset) {
                Some(value) => values.push(value.clone()),
                None => return Ok(None), // rid is past the last row in this group
            }
        }
        Ok(Some(Record { values }))
    }

    fn update(&mut self, table: &TableHandle, rid: Rid, record: Record) -> Result<Rid, Error> {
        // Validate the new record before any state change so a bad payload
        // can't leave the old row tombstoned with no replacement.
        validate_record(&table.schema, &record)?;
        self.delete(table, rid)?;
        let new_rid = self.insert(table, record)?;
        tracing::debug!(
            table = %table.name,
            old_rid = rid.0,
            new_rid = new_rid.0,
            "updated row"
        );
        Ok(new_rid)
    }

    fn delete(&mut self, table: &TableHandle, rid: Rid) -> Result<(), Error> {
        let next_rid = self.catalog.describe_table(&table.name)?.next_rid;
        if rid.0 >= next_rid {
            return Err(Error::invalid_value(format!(
                "no such record: rid {} in table '{}'",
                rid.0, table.name
            )));
        }
        let rgs = u64::from(table.row_group_size);
        let group = (rid.0 / rgs) as u32;
        let offset = (rid.0 % rgs) as usize;
        let rg_dir = row_group_dir(&table.dir, group);
        let bm_path = rg_dir.join(delete_bitmap::FILE_NAME);
        let (_, bitmap) = delete_bitmap::load(&bm_path)?;
        if delete_bitmap::is_deleted(&bitmap, offset) {
            return Err(Error::invalid_value(format!(
                "no such record: rid {} in table '{}'",
                rid.0, table.name
            )));
        }
        delete_bitmap::mark_deleted(&bm_path, offset)?;

        // Refresh INT min/max for the affected group so future skip-pruning
        // sees a tight live range. The bitmap is reloaded — mark_deleted
        // updated it on disk — and each INT column is rewritten with the
        // same data area, only its header stats change.
        let (_, deletes) = delete_bitmap::load(&bm_path)?;
        for col in &table.schema.columns {
            if !matches!(col.ty, ColumnType::Int) {
                continue;
            }
            let path = col_path(&rg_dir, &col.name);
            let (_, values) = column_file::read_column(&path)?;
            column_file::write_column(&path, col.ty, &values, &deletes)?;
        }

        tracing::debug!(table = %table.name, rid = rid.0, "deleted row");
        Ok(())
    }

    fn scan<'a>(&'a self, table: &TableHandle) -> Result<ScanIter<'a>, Error> {
        // Snapshot `next_rid` once at scan-start so a concurrent insert can't
        // make us yield a row whose data we never planned to read.
        let total_rows = self.catalog.describe_table(&table.name)?.next_rid;
        let state = ScanState {
            table_dir: table.dir.clone(),
            columns: table.schema.columns.clone(),
            row_group_size: table.row_group_size,
            total_rows,
            current_group: 0,
            next_offset_in_group: 0,
            loaded_group: None,
            errored: false,
        };
        Ok(Box::new(state))
    }
}

/// One row group loaded into memory for the duration of an in-flight scan.
/// Held one-at-a-time inside `ScanState` so memory usage tops out at the
/// rows of a single row group, not the whole table.
struct LoadedGroup {
    /// Per-column decoded values, in `columns` order.
    columns: Vec<Vec<Value>>,
    /// Raw bitmap bytes from `deletes.bm` (header already stripped).
    deletes: Vec<u8>,
    /// Number of live offsets in this group; comes from any column's header
    /// since every column in a group has identical row counts.
    row_count: usize,
}

/// Streaming scan iterator. Lazily loads each row group on demand, walks
/// offsets within the group, and stops when `current_group * row_group_size`
/// reaches the snapshotted `total_rows`. After a load failure, `errored`
/// pins it shut so the caller can't accidentally resume past corruption.
struct ScanState {
    table_dir: PathBuf,
    columns: Vec<Column>,
    row_group_size: u32,
    total_rows: u64,

    current_group: u32,
    next_offset_in_group: usize,
    loaded_group: Option<LoadedGroup>,
    errored: bool,
}

impl ScanState {
    fn load_current_group(&mut self) -> Result<(), Error> {
        let rg_dir = row_group_dir(&self.table_dir, self.current_group);
        let mut columns = Vec::with_capacity(self.columns.len());
        for col in &self.columns {
            let (_, values) = column_file::read_column(&col_path(&rg_dir, &col.name))?;
            columns.push(values);
        }
        let (_, deletes) = delete_bitmap::load(&rg_dir.join(delete_bitmap::FILE_NAME))?;
        // All columns in a group carry identical row counts; take the first.
        let row_count = columns.first().map_or(0, |c| c.len());
        self.loaded_group = Some(LoadedGroup {
            columns,
            deletes,
            row_count,
        });
        self.next_offset_in_group = 0;
        Ok(())
    }
}

impl Iterator for ScanState {
    type Item = Result<(Rid, Record), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.errored {
            return None;
        }
        let rgs = u64::from(self.row_group_size);
        loop {
            let first_rid_of_group = u64::from(self.current_group) * rgs;
            if first_rid_of_group >= self.total_rows {
                return None;
            }
            if self.loaded_group.is_none()
                && let Err(e) = self.load_current_group()
            {
                self.errored = true;
                return Some(Err(e));
            }
            let group = self.loaded_group.as_ref().unwrap();
            while self.next_offset_in_group < group.row_count {
                let offset = self.next_offset_in_group;
                self.next_offset_in_group += 1;
                let rid = first_rid_of_group + offset as u64;
                if rid >= self.total_rows {
                    // Guard against catalog/disk drift: header says more rows
                    // than the snapshotted next_rid covers.
                    return None;
                }
                if delete_bitmap::is_deleted(&group.deletes, offset) {
                    continue;
                }
                let values = group.columns.iter().map(|c| c[offset].clone()).collect();
                return Some(Ok((Rid(rid), Record { values })));
            }
            // Group exhausted — advance and let the next iteration load.
            self.loaded_group = None;
            self.current_group += 1;
        }
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
            crate::checksum::wrap(
                b"magic = \"sqlite\"\nformat_version = 1\ncreated = \"unix:0\"\n",
            ),
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
        let (id_h, _) = column_file::read_column(&rg0.join("id.col")).unwrap();
        assert_eq!(id_h.logical_type, ColumnType::Int);
        assert_eq!(id_h.row_count, 0);
        assert_eq!(id_h.null_count, 0);
        assert!(!id_h.has_nulls());

        let (name_h, _) = column_file::read_column(&rg0.join("name.col")).unwrap();
        assert_eq!(name_h.logical_type, ColumnType::Text);
        assert_eq!(name_h.row_count, 0);

        let bm_path = rg0.join(delete_bitmap::FILE_NAME);
        assert!(bm_path.is_file(), "deletes.bm should be materialized");
        let (bm_h, _) = delete_bitmap::load(&bm_path).unwrap();
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
            .insert(
                &h,
                record(vec![Value::Int(1), Value::Text("alice".to_string())]),
            )
            .unwrap();
        let r1 = store
            .insert(&h, record(vec![Value::Int(2), Value::Null]))
            .unwrap();
        assert_eq!(r0, Rid(0));
        assert_eq!(r1, Rid(1));

        assert_eq!(
            store.get(&h, Rid(0)).unwrap(),
            Some(record(vec![
                Value::Int(1),
                Value::Text("alice".to_string())
            ]))
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
                .insert(
                    &h,
                    record(vec![Value::Int(i), Value::Text(format!("n{i}"))]),
                )
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
                .insert(
                    &h,
                    record(vec![Value::Int(7), Value::Text("zed".to_string())]),
                )
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
    fn update_reassigns_rid_and_returns_new_value() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let h = store.open_table("users").unwrap();
        let old = store
            .insert(
                &h,
                record(vec![Value::Int(1), Value::Text("alice".to_string())]),
            )
            .unwrap();
        let new = store
            .update(
                &h,
                old,
                record(vec![Value::Int(1), Value::Text("alicia".to_string())]),
            )
            .unwrap();
        assert_ne!(new, old);
        assert_eq!(store.get(&h, old).unwrap(), None);
        assert_eq!(
            store.get(&h, new).unwrap(),
            Some(record(vec![
                Value::Int(1),
                Value::Text("alicia".to_string())
            ]))
        );
    }

    #[test]
    fn update_replaces_row_in_scan_with_new_rid() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let h = store.open_table("users").unwrap();
        let r0 = store
            .insert(&h, record(vec![Value::Int(1), Value::Null]))
            .unwrap();
        store
            .insert(&h, record(vec![Value::Int(2), Value::Null]))
            .unwrap();
        let new = store
            .update(&h, r0, record(vec![Value::Int(99), Value::Null]))
            .unwrap();
        let rows: Vec<(u64, i64)> = collect_scan(&store, &h)
            .into_iter()
            .map(|(rid, rec)| match rec.values[0] {
                Value::Int(n) => (rid.0, n),
                _ => unreachable!(),
            })
            .collect();
        // r0 is tombstoned, the new row sits at the tail with rid `new`.
        assert_eq!(rows, vec![(1, 2), (new.0, 99)]);
    }

    #[test]
    fn update_unknown_rid_returns_error() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let h = store.open_table("users").unwrap();
        let err = store
            .update(&h, Rid(0), record(vec![Value::Int(1), Value::Null]))
            .unwrap_err();
        assert!(err.to_string().contains("no such record"));
    }

    #[test]
    fn update_with_bad_record_does_not_tombstone_old() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let h = store.open_table("users").unwrap();
        let rid = store
            .insert(
                &h,
                record(vec![Value::Int(1), Value::Text("alice".to_string())]),
            )
            .unwrap();
        // Wrong arity — must fail before touching the bitmap.
        let err = store
            .update(&h, rid, record(vec![Value::Int(1)]))
            .unwrap_err();
        assert!(err.to_string().contains("column(s)"));
        assert!(
            store.get(&h, rid).unwrap().is_some(),
            "old row must remain live when validation rejects the new record"
        );
    }

    #[test]
    fn update_persists_across_reopen() {
        let (_tmp, db) = init_db();
        let new_rid;
        {
            let mut store = ColumnStore::open(&db).unwrap();
            store
                .create_table("users", schema_users(), TableOptions::default())
                .unwrap();
            let h = store.open_table("users").unwrap();
            let r0 = store
                .insert(
                    &h,
                    record(vec![Value::Int(1), Value::Text("alice".to_string())]),
                )
                .unwrap();
            new_rid = store
                .update(
                    &h,
                    r0,
                    record(vec![Value::Int(1), Value::Text("alicia".to_string())]),
                )
                .unwrap();
            assert_eq!(store.get(&h, r0).unwrap(), None);
        }
        let store = ColumnStore::open(&db).unwrap();
        let h = store.open_table("users").unwrap();
        assert_eq!(store.get(&h, Rid(0)).unwrap(), None);
        assert_eq!(
            store.get(&h, new_rid).unwrap(),
            Some(record(vec![
                Value::Int(1),
                Value::Text("alicia".to_string())
            ]))
        );
    }

    #[test]
    fn delete_then_get_returns_none() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let h = store.open_table("users").unwrap();
        let rid = store
            .insert(
                &h,
                record(vec![Value::Int(1), Value::Text("alice".to_string())]),
            )
            .unwrap();
        store.delete(&h, rid).unwrap();
        assert_eq!(store.get(&h, rid).unwrap(), None);
    }

    #[test]
    fn delete_skips_row_from_scan() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let h = store.open_table("users").unwrap();
        for i in 0..3 {
            store
                .insert(&h, record(vec![Value::Int(i), Value::Null]))
                .unwrap();
        }
        store.delete(&h, Rid(1)).unwrap();
        let rids: Vec<u64> = collect_scan(&store, &h)
            .into_iter()
            .map(|(r, _)| r.0)
            .collect();
        assert_eq!(rids, vec![0, 2]);
    }

    #[test]
    fn delete_refreshes_int_min_max_in_column_header() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let h = store.open_table("users").unwrap();
        for n in [5i64, 1, 9] {
            store
                .insert(&h, record(vec![Value::Int(n), Value::Null]))
                .unwrap();
        }
        let id_path = db
            .join("tables")
            .join("00000001")
            .join("row_groups")
            .join("000000")
            .join("id.col");
        let (header, _) = column_file::read_column(&id_path).unwrap();
        assert_eq!(header.int_min(), Some(1));
        assert_eq!(header.int_max(), Some(9));

        // Tombstoning the extremes shrinks the live range.
        store.delete(&h, Rid(2)).unwrap(); // 9
        store.delete(&h, Rid(1)).unwrap(); // 1
        let (header, _) = column_file::read_column(&id_path).unwrap();
        assert_eq!(header.int_min(), Some(5));
        assert_eq!(header.int_max(), Some(5));

        // Deleting the last live row collapses the range to the sentinel.
        store.delete(&h, Rid(0)).unwrap();
        let (header, _) = column_file::read_column(&id_path).unwrap();
        assert_eq!(header.int_min(), None);
        assert_eq!(header.int_max(), None);
    }

    #[test]
    fn delete_unknown_rid_returns_error() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let h = store.open_table("users").unwrap();
        // No rows yet — every rid is past next_rid.
        let err = store.delete(&h, Rid(0)).unwrap_err();
        assert!(err.to_string().contains("no such record"));
        // One row in; rid 1 is still past next_rid.
        store
            .insert(&h, record(vec![Value::Int(1), Value::Null]))
            .unwrap();
        let err = store.delete(&h, Rid(1)).unwrap_err();
        assert!(err.to_string().contains("no such record"));
    }

    #[test]
    fn delete_already_deleted_returns_error() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let h = store.open_table("users").unwrap();
        let rid = store
            .insert(&h, record(vec![Value::Int(1), Value::Null]))
            .unwrap();
        store.delete(&h, rid).unwrap();
        let err = store.delete(&h, rid).unwrap_err();
        assert!(err.to_string().contains("no such record"));
    }

    #[test]
    fn delete_persists_across_reopen() {
        let (_tmp, db) = init_db();
        {
            let mut store = ColumnStore::open(&db).unwrap();
            store
                .create_table("users", schema_users(), TableOptions::default())
                .unwrap();
            let h = store.open_table("users").unwrap();
            store
                .insert(&h, record(vec![Value::Int(1), Value::Null]))
                .unwrap();
            store
                .insert(&h, record(vec![Value::Int(2), Value::Null]))
                .unwrap();
            store.delete(&h, Rid(0)).unwrap();
        }
        let store = ColumnStore::open(&db).unwrap();
        let h = store.open_table("users").unwrap();
        assert_eq!(store.get(&h, Rid(0)).unwrap(), None);
        let rids: Vec<u64> = collect_scan(&store, &h)
            .into_iter()
            .map(|(r, _)| r.0)
            .collect();
        assert_eq!(rids, vec![1]);
    }

    fn collect_scan(store: &ColumnStore, h: &TableHandle) -> Vec<(Rid, Record)> {
        store
            .scan(h)
            .unwrap()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>()
    }

    #[test]
    fn scan_on_empty_table_yields_nothing() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let h = store.open_table("users").unwrap();
        assert!(collect_scan(&store, &h).is_empty());
    }

    #[test]
    fn scan_returns_inserted_rows_in_rid_order() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let h = store.open_table("users").unwrap();
        store
            .insert(
                &h,
                record(vec![Value::Int(1), Value::Text("alice".to_string())]),
            )
            .unwrap();
        store
            .insert(&h, record(vec![Value::Int(2), Value::Null]))
            .unwrap();
        store
            .insert(
                &h,
                record(vec![Value::Int(3), Value::Text("carol".to_string())]),
            )
            .unwrap();

        let rows = collect_scan(&store, &h);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].0, Rid(0));
        assert_eq!(rows[2].0, Rid(2));
        assert_eq!(rows[1].1.values, vec![Value::Int(2), Value::Null]);
    }

    #[test]
    fn scan_walks_multiple_row_groups() {
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
                .insert(
                    &h,
                    record(vec![Value::Int(i), Value::Text(format!("n{i}"))]),
                )
                .unwrap();
        }
        let rows = collect_scan(&store, &h);
        let rids: Vec<u64> = rows.iter().map(|(r, _)| r.0).collect();
        assert_eq!(rids, vec![0, 1, 2, 3, 4]);
        for (i, (_, rec)) in rows.iter().enumerate() {
            assert_eq!(rec.values[0], Value::Int(i as i64));
            assert_eq!(rec.values[1], Value::Text(format!("n{i}")));
        }
    }

    #[test]
    fn scan_skips_offsets_marked_deleted_in_bitmap() {
        // Flip a raw bitmap byte to prove the scan honors a tombstone even
        // when the bit wasn't set through the delete API.
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let h = store.open_table("users").unwrap();
        for i in 0..3 {
            store
                .insert(&h, record(vec![Value::Int(i), Value::Null]))
                .unwrap();
        }

        let bm_path = db
            .join("tables")
            .join("00000001")
            .join("row_groups")
            .join("000000")
            .join(delete_bitmap::FILE_NAME);
        let mut bytes = std::fs::read(&bm_path).unwrap();
        // Mark rid 1 deleted: bit 1 of the first bitmap byte (after the 24-byte header).
        bytes[24] |= 0b0000_0010;
        std::fs::write(&bm_path, bytes).unwrap();

        let rids: Vec<u64> = collect_scan(&store, &h)
            .into_iter()
            .map(|(r, _)| r.0)
            .collect();
        assert_eq!(rids, vec![0, 2]);
    }

    #[test]
    fn scan_results_survive_reopen() {
        let (_tmp, db) = init_db();
        {
            let mut store = ColumnStore::open(&db).unwrap();
            store
                .create_table("users", schema_users(), TableOptions::default())
                .unwrap();
            let h = store.open_table("users").unwrap();
            store
                .insert(
                    &h,
                    record(vec![Value::Int(7), Value::Text("zed".to_string())]),
                )
                .unwrap();
            store
                .insert(&h, record(vec![Value::Int(8), Value::Null]))
                .unwrap();
        }
        let store = ColumnStore::open(&db).unwrap();
        let h = store.open_table("users").unwrap();
        let rows = collect_scan(&store, &h);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1.values[0], Value::Int(7));
        assert_eq!(rows[1].1.values[1], Value::Null);
    }
}
