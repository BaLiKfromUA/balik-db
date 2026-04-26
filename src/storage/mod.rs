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
//! Stage 1 implements only the catalog-level methods (`create_table`,
//! `open_table`, `list_tables`, `describe_table`, `drop_table`). The
//! data-plane methods (`insert`, `get`, `update`, `delete`, `scan`) are
//! declared on the trait so callers can compile against the final contract;
//! their bodies panic with `unimplemented!` until Stage 2.

pub mod column_file;
pub mod column_store;

use std::path::PathBuf;

use crate::catalog::schema::Schema;
use crate::catalog::tables::{TableDescriptor, TableId, TableOptions};
use crate::error::Error;

/// Opaque positional record identifier. Maps internally to
/// `(row_group_id, offset_in_group)` via the table's `row_group_size`. Stable
/// while the table is append-only; once delete/update arrive in later stages,
/// a per-row-group delete bitmap will track holes — RIDs themselves never
/// shift or get reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Rid(pub u64);

/// Lightweight reference to a loaded table — what data-plane operations
/// take. Cheap to clone. `TableDescriptor` (in `catalog::tables`) is the
/// richer human-facing form returned by `describe_table`.
#[derive(Debug, Clone)]
pub struct TableHandle {
    pub id: TableId,
    pub name: String,
    pub schema: Schema,
    pub dir: PathBuf,
    pub row_group_size: u32,
}

/// One typed value matching a logical column type. Stage 1 placeholder —
/// Stage 2 wires this into the column-file encoding/decoding layer.
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

    // ---- Stage 2+ data plane (declared now, implemented later) ----

    fn insert(&mut self, table: &TableHandle, record: Record) -> Result<Rid, Error>;

    fn get(&self, table: &TableHandle, rid: Rid) -> Result<Option<Record>, Error>;

    fn update(&mut self, table: &TableHandle, rid: Rid, record: Record) -> Result<(), Error>;

    fn delete(&mut self, table: &TableHandle, rid: Rid) -> Result<(), Error>;

    fn scan<'a>(
        &'a self,
        table: &TableHandle,
    ) -> Result<Box<dyn Iterator<Item = Result<(Rid, Record), Error>> + 'a>, Error>;
}
