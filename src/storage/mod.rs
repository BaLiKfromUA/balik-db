//! Common storage API used by the parser and query engine layers.
//!
//! Layering:
//!
//! ```text
//!   parser / query engine        (later stages)
//!         │
//!   storage::Storage trait       (this module — the stable contract)
//!         │
//!   storage::column_store        (Track B implementation)
//!         │
//!   catalog::Catalog             (on-disk schema + table directory)
//! ```
//!
//! All catalog-level methods (`create_table`, `open_table`, `list_tables`,
//! `describe_table`, `drop_table`) and data-plane methods (`insert`, `get`,
//! `scan`, `update`, `delete`) are implemented.

pub mod column_file;
pub mod column_store;
pub mod delete_bitmap;

use std::path::PathBuf;

use serde::Serialize;

use crate::catalog::schema::Schema;
use crate::catalog::tables::{TableDescriptor, TableId, TableOptions};
use crate::error::Error;

/// Opaque positional record identifier. Maps internally to
/// `(row_group_id, offset_in_group)` via the table's `row_group_size`. Stable
/// while the table is append-only; once delete/update land, a per-row-group
/// delete bitmap tracks holes — RIDs themselves never shift or get reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Rid(pub u64);

/// Lightweight reference to a loaded table — what data-plane operations
/// take. Cheap to clone. `TableDescriptor` (in `catalog::tables`) is the
/// richer human-facing form returned by `describe_table`.
#[derive(Debug, Clone)]
pub struct TableHandle {
    /// Carried for identity/logging; data-plane lookups go through `name`.
    #[allow(dead_code)]
    pub id: TableId,
    pub name: String,
    pub schema: Schema,
    pub dir: PathBuf,
    pub row_group_size: u32,
}

/// One typed value matching a logical column type. The variant must line up
/// with the column's `ColumnType`; `Null` is allowed only on nullable columns.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Text(String),
    Null,
}

/// One row, ordered to match `TableHandle::schema.columns`.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub values: Vec<Value>,
}

/// A column-major batch of rows — the unit the vectorized scan produces and
/// query operators pass between each other. `columns[c]` holds every value of
/// the column named `names[c]`; all column vectors share the same length, the
/// batch's row count. Deleted rows are already compacted out by the scan.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnBatch {
    pub names: Vec<String>,
    pub columns: Vec<Vec<Value>>,
}

impl ColumnBatch {
    /// Number of rows in the batch (length of any column vector).
    pub fn num_rows(&self) -> usize {
        self.columns.first().map_or(0, Vec::len)
    }
}

/// Comparison operator for a [`ScanPredicate`]. A storage-local mirror of the
/// parser's comparison operators, kept here so the storage layer takes no
/// dependency on the AST.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ScanCompare {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

/// A `column <op> constant` hint the scan can use to skip whole row groups via
/// their INT `min`/`max` zone maps. Only INT columns carry stats, so the
/// constant is an `i64`. Pruning is a pure optimization — a scan that ignored
/// every predicate would still be correct — so callers may pass any conjunctive
/// subset they can extract.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScanPredicate {
    pub column: String,
    pub op: ScanCompare,
    pub value: i64,
}

/// Iterator returned by `Storage::scan`. Aliased so the trait signature
/// doesn't trip clippy's `type_complexity` lint. The `+ 'a` bound ties the
/// iterator's lifetime to the `&self` borrow that produced it.
#[allow(dead_code)] // row-oriented scan: see `Storage::scan`
pub type ScanIter<'a> = Box<dyn Iterator<Item = Result<(Rid, Record), Error>> + 'a>;

/// Iterator returned by `Storage::scan_batches`: one [`ColumnBatch`] per live
/// row group. Same lifetime contract as [`ScanIter`].
pub type BatchIter<'a> = Box<dyn Iterator<Item = Result<ColumnBatch, Error>> + 'a>;

pub trait Storage {
    fn create_table(
        &mut self,
        name: &str,
        schema: Schema,
        options: TableOptions,
    ) -> Result<TableId, Error>;

    fn open_table(&self, name: &str) -> Result<TableHandle, Error>;

    fn list_tables(&self) -> Result<Vec<String>, Error>;

    fn describe_table(&self, name: &str) -> Result<TableDescriptor, Error>;

    fn drop_table(&mut self, name: &str) -> Result<(), Error>;

    // ---- data plane ----

    fn insert(&mut self, table: &TableHandle, record: Record) -> Result<Rid, Error>;

    fn get(&self, table: &TableHandle, rid: Rid) -> Result<Option<Record>, Error>;

    /// Replace the row at `rid` with `record`. Because updates are modeled
    /// as `delete + insert`, the row's positional rid is reassigned — the
    /// new rid is returned so callers can re-read or reference the row.
    fn update(&mut self, table: &TableHandle, rid: Rid, record: Record) -> Result<Rid, Error>;

    fn delete(&mut self, table: &TableHandle, rid: Rid) -> Result<(), Error>;

    /// Row-oriented full scan yielding `(rid, record)` for every live row.
    /// No CLI command wires this anymore — reads go through SQL `SELECT`, which
    /// uses the vectorized [`Storage::scan_batches`]. It is kept because storage
    /// tests use it to assert rid-level read-back, and a future physical-plan
    /// implementation of UPDATE / DELETE (which must visit rows by rid) is the
    /// natural place to reuse it.
    #[allow(dead_code)]
    fn scan<'a>(&'a self, table: &TableHandle) -> Result<ScanIter<'a>, Error>;

    /// Vectorized scan: yield one column-major [`ColumnBatch`] per live row
    /// group. `projection` selects which columns to decode (`None` = all, in
    /// schema order); `prune` carries zone-map hints used to skip row groups
    /// whose INT `min`/`max` prove no row can match. Deleted rows are compacted
    /// out of each batch, and a row group with no live rows yields no batch.
    fn scan_batches<'a>(
        &'a self,
        table: &TableHandle,
        projection: Option<&[String]>,
        prune: &[ScanPredicate],
    ) -> Result<BatchIter<'a>, Error>;
}
