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
    BatchIter, ColumnBatch, Record, Rid, ScanCompare, ScanIter, ScanPredicate, Storage,
    TableHandle, Value, column_file, delete_bitmap,
};

const ROW_GROUPS_DIR: &str = "row_groups";

/// Whole-database advisory lock file, held open for the lifetime of an open
/// store. Lives in the db root next to `catalog.toml`.
const LOCK_FILE: &str = "balik.lock";

/// Take an exclusive advisory lock on the database for the lifetime of the
/// returned file handle. The lock is held per open file description, so two
/// processes (or two `ColumnStore`s) opening the same database can't interleave
/// the read-modify-write cycles that the whole-file-rewrite layout depends on.
/// It is released automatically when the handle is dropped or the process
/// exits — even on a crash — so a killed process never leaves a stale lock.
fn acquire_lock(db_root: &Path) -> Result<fs::File, Error> {
    let path = db_root.join(LOCK_FILE);
    let file = fs::File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| Error::io("open database lock file", e))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => Err(Error::other(format!(
            "database at '{}' is already open in another process",
            db_root.display()
        ))),
        Err(fs::TryLockError::Error(e)) => Err(Error::io("lock database", e)),
    }
}

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
    /// Held open for the store's lifetime; dropping it releases the
    /// whole-database lock taken in `open`. Never read after acquisition.
    #[allow(dead_code)]
    lock: fs::File,
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

        // Lock before loading so the catalog read and the recovery rewrites
        // below happen under the same exclusive hold.
        let lock = acquire_lock(db_root)?;
        let store = Self {
            catalog: Catalog::load(db_root)?,
            lock,
        };
        store.recover()?;
        Ok(store)
    }

    /// Repair any row group left half-written by a crash mid-insert. Run once
    /// at open while the database lock is held, before any data-plane call.
    fn recover(&self) -> Result<(), Error> {
        let names: Vec<String> = self
            .catalog
            .list_tables()
            .into_iter()
            .map(String::from)
            .collect();
        for name in names {
            // A table whose manifest can't be read is already broken and can't
            // be reconciled or used; skip it so one corrupt table doesn't fail
            // the whole database open. The access path (and `doctor`) report it
            // per-table. Once the manifest reads, a reconciliation error is a
            // real repair failure and propagates.
            let handle = match self.open_table(&name) {
                Ok(handle) => handle,
                Err(e) => {
                    tracing::warn!(
                        table = %name,
                        error = %e,
                        "skipping crash recovery for unreadable table"
                    );
                    continue;
                }
            };
            self.reconcile_open_group(&handle)?;
        }
        Ok(())
    }

    /// Reconcile a table's open (last) row group against the committed
    /// `next_rid`.
    ///
    /// `insert` rewrites each column file under its own atomic rename and only
    /// advances `next_rid` after every column is durable. A crash partway
    /// through therefore leaves a prefix of the open group's columns carrying
    /// one extra, uncommitted row while `next_rid` still points before it.
    /// Truncating every column back to the committed length makes all columns
    /// agree again and drops the half-written row, so `get`/`scan` never see a
    /// torn row and never index past a short column.
    fn reconcile_open_group(&self, table: &TableHandle) -> Result<(), Error> {
        let rgs = u64::from(table.row_group_size);
        let next_rid = self.catalog.describe_table(&table.name)?.next_rid;
        let group = (next_rid / rgs) as u32;
        let rg_dir = row_group_dir(&table.dir, group);
        // The open group may not be materialized yet — an empty table, or a
        // `next_rid` sitting exactly on a group boundary. Nothing to reconcile.
        if !rg_dir.is_dir() {
            return Ok(());
        }
        let expected = (next_rid % rgs) as usize;
        let (_, deletes) = delete_bitmap::load(&rg_dir.join(delete_bitmap::FILE_NAME))?;
        for col in &table.schema.columns {
            let path = col_path(&rg_dir, &col.name);
            let have = column_file::read_header(&path)?.row_count as usize;
            if have == expected {
                continue;
            }
            if have < expected {
                return Err(Error::corrupt(format!(
                    "column '{}' in row group {group} of table '{}' holds {have} rows but the catalog committed {expected}",
                    col.name, table.name
                )));
            }
            column_file::truncate_column(&path, col.ty, expected, &deletes)?;
            tracing::warn!(
                table = %table.name,
                group,
                column = %col.name,
                had = have,
                kept = expected,
                "dropped uncommitted rows from open row group during crash recovery"
            );
        }
        Ok(())
    }

    /// Append records to a table, writing each column file once per row group
    /// rather than once per row.
    ///
    /// `insert` rewrites every column file on each call, so filling a row group
    /// of size `R` costs `O(R²)` write traffic. This path encodes each column
    /// once per group, making a load of `N` rows `O(N)` — the difference between
    /// gigabytes and terabytes of I/O when generating large datasets.
    ///
    /// It appends starting from the table's current `next_rid`, which must sit on
    /// a row-group boundary, so a large load can be streamed as a sequence of
    /// row-group-sized calls to bound peak memory. (Pass chunks whose length is a
    /// multiple of `row_group_size`, except optionally the final call, to keep
    /// every call boundary-aligned.) It is a bulk loader, not a general write
    /// path: it ignores deletes and is not crash-atomic across groups, so it is
    /// meant for loading into a freshly created table.
    pub fn bulk_load(&mut self, table: &TableHandle, records: &[Record]) -> Result<(), Error> {
        for record in records {
            validate_record(&table.schema, record)?;
        }

        let rgs = u64::from(table.row_group_size);
        let start_rid = self.catalog.describe_table(&table.name)?.next_rid;
        if start_rid % rgs != 0 {
            return Err(Error::other(format!(
                "bulk_load must start on a row-group boundary, but next_rid is {start_rid}"
            )));
        }
        let start_group = (start_rid / rgs) as u32;

        for (chunk_index, chunk) in records.chunks(rgs as usize).enumerate() {
            let group = start_group + chunk_index as u32;
            let rg_dir = row_group_dir(&table.dir, group);
            // Group 0 is materialized at create time; later groups open here.
            if group > 0 {
                materialize_row_group(&rg_dir, &table.schema.columns, table.row_group_size)?;
            }

            // A freshly loaded group has no tombstones, but the encoder still
            // wants the bitmap to derive INT min/max over live rows only.
            let (_, deletes) = delete_bitmap::load(&rg_dir.join(delete_bitmap::FILE_NAME))?;

            for (i, col) in table.schema.columns.iter().enumerate() {
                let values: Vec<Value> = chunk.iter().map(|r| r.values[i].clone()).collect();
                column_file::write_column(
                    &col_path(&rg_dir, &col.name),
                    col.ty,
                    &values,
                    &deletes,
                )?;
            }
        }

        self.catalog
            .set_next_rid(&table.name, start_rid + records.len() as u64)?;
        tracing::debug!(table = %table.name, rows = records.len(), "bulk-loaded rows");
        Ok(())
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

        // Advance the allocator only after every column is durable. This commits
        // the row: a crash before this point leaves the columns rewritten but
        // `next_rid` unmoved, so the row is uncommitted. The column files are
        // rewritten one atomic rename at a time, so a crash mid-loop can leave a
        // prefix of columns one row longer than the rest — `reconcile_open_group`
        // truncates them back to `next_rid` on the next open.
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

    fn scan_batches<'a>(
        &'a self,
        table: &TableHandle,
        projection: Option<&[String]>,
        prune: &[ScanPredicate],
    ) -> Result<BatchIter<'a>, Error> {
        // Resolve the projection to concrete column names in the requested
        // order; `None` decodes every column in schema order.
        let projected: Vec<String> = match projection {
            None => table
                .schema
                .columns
                .iter()
                .map(|c| c.name.clone())
                .collect(),
            Some(names) => {
                for name in names {
                    if !table.schema.columns.iter().any(|c| &c.name == name) {
                        return Err(Error::other(format!(
                            "scan projection references unknown column '{name}' in table '{}'",
                            table.name
                        )));
                    }
                }
                names.to_vec()
            }
        };

        // Keep only prune predicates on INT columns that exist — others carry
        // no zone-map stats, so they simply contribute no pruning.
        let prune: Vec<ScanPredicate> = prune
            .iter()
            .filter(|p| {
                table
                    .schema
                    .columns
                    .iter()
                    .any(|c| c.name == p.column && matches!(c.ty, ColumnType::Int))
            })
            .cloned()
            .collect();

        // Snapshot `next_rid` once so a concurrent insert can't widen the scan.
        let total_rows = self.catalog.describe_table(&table.name)?.next_rid;
        tracing::debug!(
            table = %table.name,
            projection = ?projected,
            prune = prune.len(),
            "starting scan"
        );
        Ok(Box::new(BatchScanState {
            table_dir: table.dir.clone(),
            projected,
            prune,
            row_group_size: table.row_group_size,
            total_rows,
            current_group: 0,
            errored: false,
            pruned_groups: 0,
            scanned_groups: 0,
            summarized: false,
        }))
    }
}

/// Streaming vectorized scan. For each row group it first consults the INT
/// zone maps to decide whether the group can be skipped wholesale, then decodes
/// only the projected columns and compacts out deleted rows into one batch.
struct BatchScanState {
    table_dir: PathBuf,
    projected: Vec<String>,
    prune: Vec<ScanPredicate>,
    row_group_size: u32,
    total_rows: u64,
    current_group: u32,
    errored: bool,
    /// Row groups skipped wholesale via the zone maps, and those actually
    /// decoded — counted for the scan-completion summary log.
    pruned_groups: u32,
    scanned_groups: u32,
    /// Guards the one-shot completion summary so re-polling an exhausted scan
    /// doesn't log it again.
    summarized: bool,
}

impl BatchScanState {
    /// Return the next non-empty batch, or `None` once every group is consumed.
    fn try_next(&mut self) -> Result<Option<ColumnBatch>, Error> {
        let rgs = u64::from(self.row_group_size);
        loop {
            let first_rid_of_group = u64::from(self.current_group) * rgs;
            if first_rid_of_group >= self.total_rows {
                if !self.summarized {
                    self.summarized = true;
                    tracing::debug!(
                        pruned = self.pruned_groups,
                        scanned = self.scanned_groups,
                        "scan complete"
                    );
                }
                return Ok(None);
            }
            let rg_dir = row_group_dir(&self.table_dir, self.current_group);
            self.current_group += 1;

            if self.group_pruned(&rg_dir)? {
                self.pruned_groups += 1;
                continue;
            }
            self.scanned_groups += 1;
            if let Some(batch) = self.load_group(&rg_dir, first_rid_of_group)? {
                return Ok(Some(batch));
            }
        }
    }

    /// True when the group's INT zone maps prove no live row can satisfy every
    /// (conjunctive) prune predicate, so the group can be skipped without
    /// decoding any data.
    fn group_pruned(&self, rg_dir: &Path) -> Result<bool, Error> {
        for pred in &self.prune {
            let header = column_file::read_header(&col_path(rg_dir, &pred.column))?;
            match (header.int_min(), header.int_max()) {
                // No live INT value in this group → a comparison matches nothing.
                (Some(min), Some(max)) if group_can_match(pred.op, pred.value, min, max) => {}
                _ => {
                    tracing::trace!(
                        group = %rg_dir.display(),
                        column = %pred.column,
                        "row group pruned by zone map"
                    );
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Decode the projected columns of one group and compact out deleted rows.
    /// Returns `None` when the group has no live rows.
    fn load_group(
        &self,
        rg_dir: &Path,
        first_rid_of_group: u64,
    ) -> Result<Option<ColumnBatch>, Error> {
        let mut raw = Vec::with_capacity(self.projected.len());
        for name in &self.projected {
            let (_, values) = column_file::read_column(&col_path(rg_dir, name))?;
            raw.push(values);
        }
        let stored_rows = raw.first().map_or(0, Vec::len);
        if let Some(bad) = raw.iter().position(|c| c.len() != stored_rows) {
            return Err(Error::corrupt(format!(
                "row group at '{}': column '{}' has {} rows but '{}' has {}",
                rg_dir.display(),
                self.projected[bad],
                raw[bad].len(),
                self.projected[0],
                stored_rows,
            )));
        }
        // Never read past the snapshotted commit point, even if a column file
        // physically holds more rows (e.g. an unrecovered torn write).
        let remaining = (self.total_rows - first_rid_of_group) as usize;
        let group_rows = stored_rows.min(remaining);

        let (_, deletes) = delete_bitmap::load(&rg_dir.join(delete_bitmap::FILE_NAME))?;
        let mut columns: Vec<Vec<Value>> = vec![Vec::new(); raw.len()];
        for offset in 0..group_rows {
            if delete_bitmap::is_deleted(&deletes, offset) {
                continue;
            }
            for (c, col) in raw.iter().enumerate() {
                columns[c].push(col[offset].clone());
            }
        }
        if columns.first().is_some_and(Vec::is_empty) {
            return Ok(None);
        }
        Ok(Some(ColumnBatch {
            names: self.projected.clone(),
            columns,
        }))
    }
}

impl Iterator for BatchScanState {
    type Item = Result<ColumnBatch, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.errored {
            return None;
        }
        match self.try_next() {
            Ok(Some(batch)) => Some(Ok(batch)),
            Ok(None) => None,
            Err(e) => {
                self.errored = true;
                Some(Err(e))
            }
        }
    }
}

/// Whether a row group whose live INT range is `[min, max]` could contain a row
/// satisfying `col <op> value`. Conservative: `true` keeps the group, `false`
/// proves it can be skipped.
fn group_can_match(op: ScanCompare, value: i64, min: i64, max: i64) -> bool {
    match op {
        ScanCompare::Eq => min <= value && value <= max,
        // Only impossible when every live value equals `value`.
        ScanCompare::NotEq => !(min == value && max == value),
        ScanCompare::Lt => min < value,
        ScanCompare::LtEq => min <= value,
        ScanCompare::Gt => max > value,
        ScanCompare::GtEq => max >= value,
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
        // All columns in a group must carry identical row counts. A mismatch
        // means corruption or an interrupted write that recovery didn't repair;
        // surface it instead of indexing a short column out of bounds below.
        let row_count = columns.first().map_or(0, |c| c.len());
        if let Some(bad) = columns.iter().position(|c| c.len() != row_count) {
            return Err(Error::corrupt(format!(
                "row group {} of '{}': column '{}' has {} rows but '{}' has {}",
                self.current_group,
                self.table_dir.display(),
                self.columns[bad].name,
                columns[bad].len(),
                self.columns[0].name,
                row_count,
            )));
        }
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

    fn collect_batches(
        store: &ColumnStore,
        h: &TableHandle,
        projection: Option<&[String]>,
        prune: &[ScanPredicate],
    ) -> Vec<ColumnBatch> {
        store
            .scan_batches(h, projection, prune)
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn bulk_load_spans_row_groups_and_scans_back_in_order() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table(
                "users",
                schema_users(),
                TableOptions {
                    row_group_size: Some(2),
                },
            )
            .unwrap();
        let h = store.open_table("users").unwrap();

        // rgs=2 with 5 rows -> groups [0,1], [2,3], [4]; the last group is
        // partially filled, exercising the short final chunk.
        let records: Vec<Record> = (0..5)
            .map(|i| record(vec![Value::Int(i), Value::Text(format!("name{i}"))]))
            .collect();
        store.bulk_load(&h, &records).unwrap();

        // next_rid lands at the row count, so point reads see every row...
        assert_eq!(store.describe_table("users").unwrap().next_rid, 5);
        for i in 0..5 {
            let got = store.get(&h, Rid(i)).unwrap().unwrap();
            assert_eq!(got.values[0], Value::Int(i as i64));
        }

        // ...and a scan returns all rows across all three groups in rid order.
        let rows: Vec<Value> = collect_batches(&store, &h, Some(&["id".to_string()]), &[])
            .into_iter()
            .flat_map(|b| b.columns[0].clone())
            .collect();
        assert_eq!(
            rows,
            (0..5).map(Value::Int).collect::<Vec<_>>(),
            "every bulk-loaded row scanned back in order"
        );
    }

    #[test]
    fn bulk_load_appends_across_row_group_aligned_calls() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table(
                "users",
                schema_users(),
                TableOptions {
                    row_group_size: Some(2),
                },
            )
            .unwrap();
        let h = store.open_table("users").unwrap();

        // Two boundary-aligned calls (each a full group of 2) then a final
        // short call, streaming the load instead of buffering all of it.
        let mk = |ids: std::ops::Range<i64>| -> Vec<Record> {
            ids.map(|i| record(vec![Value::Int(i), Value::Null]))
                .collect()
        };
        store.bulk_load(&h, &mk(0..2)).unwrap();
        store.bulk_load(&h, &mk(2..4)).unwrap();
        store.bulk_load(&h, &mk(4..5)).unwrap();

        assert_eq!(store.describe_table("users").unwrap().next_rid, 5);
        let rows: Vec<Value> = collect_batches(&store, &h, Some(&["id".to_string()]), &[])
            .into_iter()
            .flat_map(|b| b.columns[0].clone())
            .collect();
        assert_eq!(rows, (0..5).map(Value::Int).collect::<Vec<_>>());
    }

    #[test]
    fn bulk_load_rejects_unaligned_start() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table(
                "users",
                schema_users(),
                TableOptions {
                    row_group_size: Some(2),
                },
            )
            .unwrap();
        let h = store.open_table("users").unwrap();

        // A short first call leaves next_rid mid-group (1 % 2 != 0), so the
        // next bulk_load can't land on a clean group boundary.
        store
            .bulk_load(&h, &[record(vec![Value::Int(0), Value::Null])])
            .unwrap();
        let err = store
            .bulk_load(&h, &[record(vec![Value::Int(1), Value::Null])])
            .unwrap_err();
        assert!(err.to_string().contains("row-group boundary"));
    }

    #[test]
    fn insert_after_bulk_load_appends_at_next_rid() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let h = store.open_table("users").unwrap();

        let records: Vec<Record> = (0..3)
            .map(|i| record(vec![Value::Int(i), Value::Null]))
            .collect();
        store.bulk_load(&h, &records).unwrap();

        // A normal insert after a bulk load continues from the committed
        // next_rid rather than overwriting row 0.
        let rid = store
            .insert(&h, record(vec![Value::Int(99), Value::Text("late".into())]))
            .unwrap();
        assert_eq!(rid, Rid(3));
        assert_eq!(store.describe_table("users").unwrap().next_rid, 4);
        assert_eq!(
            store.get(&h, Rid(3)).unwrap().unwrap().values[1],
            Value::Text("late".into())
        );
    }

    #[test]
    fn scan_batches_yields_compacted_column_major_batch() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let h = store.open_table("users").unwrap();
        for (id, name) in [(1, Some("alice")), (2, None), (3, Some("carol"))] {
            let nm = name.map_or(Value::Null, |s| Value::Text(s.to_string()));
            store.insert(&h, record(vec![Value::Int(id), nm])).unwrap();
        }
        store.delete(&h, Rid(1)).unwrap();

        let batches = collect_batches(&store, &h, None, &[]);
        assert_eq!(batches.len(), 1, "single default-sized row group");
        let b = &batches[0];
        assert_eq!(b.names, vec!["id", "name"]);
        assert_eq!(b.num_rows(), 2, "deleted row compacted out");
        assert_eq!(b.columns[0], vec![Value::Int(1), Value::Int(3)]);
        assert_eq!(
            b.columns[1],
            vec![Value::Text("alice".into()), Value::Text("carol".into())]
        );
    }

    #[test]
    fn scan_batches_projection_decodes_only_requested_columns() {
        let (_tmp, db) = init_db();
        let mut store = ColumnStore::open(&db).unwrap();
        store
            .create_table("users", schema_users(), TableOptions::default())
            .unwrap();
        let h = store.open_table("users").unwrap();
        store
            .insert(&h, record(vec![Value::Int(1), Value::Text("alice".into())]))
            .unwrap();

        let batches = collect_batches(&store, &h, Some(&["name".to_string()]), &[]);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].names, vec!["name"]);
        assert_eq!(batches[0].columns.len(), 1);
        assert_eq!(batches[0].columns[0], vec![Value::Text("alice".into())]);
    }

    #[test]
    fn scan_batches_prunes_row_groups_via_int_min_max() {
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
        // rgs=2: ids 0,1 in group 0; 2,3 in group 1; 4 in group 2.
        for i in 0..5 {
            store
                .insert(&h, record(vec![Value::Int(i), Value::Null]))
                .unwrap();
        }

        // `id > 3` can only match group 2 (max id 4); groups 0 and 1 are pruned.
        let prune = vec![ScanPredicate {
            column: "id".into(),
            op: ScanCompare::Gt,
            value: 3,
        }];
        let batches = collect_batches(&store, &h, Some(&["id".to_string()]), &prune);
        let ids: Vec<Value> = batches.iter().flat_map(|b| b.columns[0].clone()).collect();
        assert_eq!(ids, vec![Value::Int(4)]);
    }

    #[test]
    fn open_recovers_torn_insert_by_truncating_uncommitted_rows() {
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
                    record(vec![Value::Int(1), Value::Text("alice".to_string())]),
                )
                .unwrap();
        }

        // Simulate a crash partway through the next insert: the id column was
        // rewritten with an extra row, but the process died before name.col was
        // rewritten and before next_rid advanced. The group is now torn — id
        // has 2 rows, name has 1, next_rid is still 1.
        let rg0 = db
            .join("tables")
            .join("00000001")
            .join("row_groups")
            .join("000000");
        let id_path = rg0.join("id.col");
        let (_, mut id_vals) = column_file::read_column(&id_path).unwrap();
        id_vals.push(Value::Int(999));
        column_file::write_column(&id_path, ColumnType::Int, &id_vals, &[]).unwrap();
        assert_eq!(column_file::read_header(&id_path).unwrap().row_count, 2);

        // Reopen runs recovery, which truncates id.col back to the committed row.
        let mut store = ColumnStore::open(&db).unwrap();
        let h = store.open_table("users").unwrap();
        assert_eq!(column_file::read_header(&id_path).unwrap().row_count, 1);

        // Scan no longer risks indexing past the short column and yields only
        // the committed row.
        let rows = collect_scan(&store, &h);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, Rid(0));
        assert_eq!(
            rows[0].1.values,
            vec![Value::Int(1), Value::Text("alice".to_string())]
        );

        // The next insert reuses the freed rid and stays column-aligned.
        let new = store
            .insert(&h, record(vec![Value::Int(2), Value::Null]))
            .unwrap();
        assert_eq!(new, Rid(1));
        assert_eq!(
            store.get(&h, Rid(1)).unwrap(),
            Some(record(vec![Value::Int(2), Value::Null]))
        );
    }

    #[test]
    fn open_rejects_a_second_concurrent_handle() {
        let (_tmp, db) = init_db();
        let _first = ColumnStore::open(&db).unwrap();
        let err = ColumnStore::open(&db).unwrap_err();
        assert!(
            err.to_string().contains("already open in another process"),
            "got: {err}"
        );
    }

    #[test]
    fn lock_is_released_when_store_is_dropped() {
        let (_tmp, db) = init_db();
        drop(ColumnStore::open(&db).unwrap());
        // The first handle released its lock on drop, so a fresh open succeeds.
        ColumnStore::open(&db).unwrap();
    }
}
