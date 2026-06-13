//! Shared fixtures for `execution` unit tests. Compiled only under `cfg(test)`,
//! so nothing here ships in a normal build.
#![cfg(test)]

use tempfile::TempDir;

use crate::catalog::metadata;
use crate::catalog::schema::{Column, ColumnType, Schema};
use crate::storage::Storage;
use crate::storage::column_store::ColumnStore;

/// A column store with a `users(id INT NOT NULL, name TEXT, age INT)` table.
///
/// The returned [`TempDir`] owns the on-disk database and must be kept alive for
/// the duration of the test; dropping it deletes the files.
pub(crate) fn seeded_store() -> (TempDir, ColumnStore) {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("db");
    metadata::initialize(&db).unwrap();
    let mut store = ColumnStore::open(&db).unwrap();
    let schema = Schema {
        columns: vec![
            Column {
                name: "id".into(),
                ty: ColumnType::Int,
                nullable: false,
            },
            Column {
                name: "name".into(),
                ty: ColumnType::Text,
                nullable: true,
            },
            Column {
                name: "age".into(),
                ty: ColumnType::Int,
                nullable: true,
            },
        ],
    };
    store
        .create_table("users", schema, Default::default())
        .unwrap();
    (tmp, store)
}
