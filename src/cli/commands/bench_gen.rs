use std::path::Path;
use std::time::Instant;

use bytesize::ByteSize;
use fastrand::Rng;

use crate::catalog::metadata::{self, Status};
use crate::catalog::schema::{Column, ColumnType, Schema};
use crate::catalog::tables::TableOptions;
use crate::error::Error;
use crate::storage::column_store::ColumnStore;
use crate::storage::{Record, Storage, Value};

/// Length, in characters, of each generated TEXT payload value.
const PAYLOAD_LEN: usize = 90;
/// Number of large TEXT payload columns the benchmark query does not select.
const PAYLOAD_COUNT: usize = 3;
/// Number of INT columns (id + sort key + two filter columns).
const INT_COUNT: usize = 4;
/// Rough on-disk bytes per row, used to turn a target `--size` into a row count:
/// each INT is 8 bytes, each raw TEXT value is a 4-byte offset plus its bytes.
const EST_BYTES_PER_ROW: u64 = (INT_COUNT * 8 + PAYLOAD_COUNT * (4 + PAYLOAD_LEN)) as u64;

/// Range for `sort_key`: wide so `ORDER BY sort_key` does real comparison work.
const SORT_RANGE: u64 = 1_000_000_000;
/// Range for `filter_a` / `filter_b`. The benchmark query filters around the
/// midpoint (`filter_a > 500000 AND filter_b < 500000`), giving ~25% selectivity.
const FILTER_RANGE: u64 = 1_000_000;

/// Generate a wide-row table of deterministic random data for benchmarking the
/// logical optimizer. Bulk-loads `--rows` rows (or as many as fit `--size`) into
/// `table`, recreating it if it already exists.
///
/// The schema is deliberately wide — four INT columns plus three large TEXT
/// payloads the benchmark query never selects — so that column pushdown has
/// something to skip and `ORDER BY ... LIMIT` has a real sort to fuse into a
/// TopK. This is a development utility, not a user-facing data path.
pub fn run(
    path: &Path,
    table: &str,
    size: &str,
    rows: Option<u64>,
    seed: u64,
) -> Result<(), Error> {
    let rows = match rows {
        Some(n) => n,
        None => (parse_size(size)? / EST_BYTES_PER_ROW).max(1),
    };

    match metadata::status(path) {
        Status::Missing => metadata::initialize(path)?,
        Status::Ok { .. } => {}
        _ => {
            return Err(Error::other(format!(
                "'{}' is not a usable balik database",
                path.display()
            )));
        }
    }

    let mut store = ColumnStore::open(path)?;
    if store.list_tables()?.iter().any(|t| t == table) {
        store.drop_table(table)?;
    }
    store.create_table(table, bench_schema(), TableOptions::default())?;
    let handle = store.open_table(table)?;

    // Generate and load one row group at a time so peak memory stays bounded
    // regardless of the target size. Each chunk is exactly one row group (the
    // last may be short), keeping every `bulk_load` call boundary-aligned.
    let group = u64::from(handle.row_group_size);
    let mut rng = Rng::with_seed(seed);
    let start = Instant::now();
    let mut next_id = 0u64;
    while next_id < rows {
        let n = (rows - next_id).min(group);
        let mut chunk = Vec::with_capacity(n as usize);
        for _ in 0..n {
            chunk.push(gen_record(next_id, &mut rng));
            next_id += 1;
        }
        store.bulk_load(&handle, &chunk)?;
    }

    println!(
        "Generated {rows} rows into table '{table}' at '{}' in {:.1}s ({} on disk)",
        path.display(),
        start.elapsed().as_secs_f64(),
        ByteSize::b(dir_size(path)),
    );
    Ok(())
}

fn bench_schema() -> Schema {
    let int = |name: &str| Column {
        name: name.to_string(),
        ty: ColumnType::Int,
        nullable: false,
    };
    let text = |name: &str| Column {
        name: name.to_string(),
        ty: ColumnType::Text,
        nullable: false,
    };
    Schema {
        columns: vec![
            int("id"),
            int("sort_key"),
            int("filter_a"),
            int("filter_b"),
            text("payload1"),
            text("payload2"),
            text("payload3"),
        ],
    }
}

fn gen_record(id: u64, rng: &mut Rng) -> Record {
    // High-cardinality random payloads so the column store keeps them raw
    // (dictionary encoding wouldn't shrink them) and the bytes hit disk.
    let payload = |rng: &mut Rng| -> Value {
        Value::Text((0..PAYLOAD_LEN).map(|_| rng.alphanumeric()).collect())
    };
    Record {
        values: vec![
            Value::Int(id as i64),
            Value::Int(rng.u64(0..SORT_RANGE) as i64),
            Value::Int(rng.u64(0..FILTER_RANGE) as i64),
            Value::Int(rng.u64(0..FILTER_RANGE) as i64),
            payload(rng),
            payload(rng),
            payload(rng),
        ],
    }
}

/// Parse a size like `1GB` / `500MiB` into bytes. Decimal units (KB, MB, GB) are
/// powers of 1000; binary units (KiB, MiB, GiB) are powers of 1024 — `bytesize`'s
/// convention.
fn parse_size(s: &str) -> Result<u64, Error> {
    s.parse::<ByteSize>()
        .map(|b| b.as_u64())
        .map_err(|e| Error::invalid_value(format!("invalid size '{s}': {e}")))
}

/// Total size of every regular file under `path`, recursively. Best-effort:
/// unreadable entries are skipped rather than failing the report.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            match entry.metadata() {
                Ok(meta) if meta.is_dir() => total += dir_size(&entry.path()),
                Ok(meta) => total += meta.len(),
                Err(_) => {}
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_size_handles_decimal_and_binary_units() {
        assert_eq!(parse_size("1GB").unwrap(), 1_000_000_000);
        assert_eq!(parse_size("1GiB").unwrap(), 1 << 30);
        assert_eq!(parse_size("250MB").unwrap(), 250_000_000);
    }

    #[test]
    fn parse_size_rejects_garbage() {
        assert!(parse_size("nope").is_err());
    }

    #[test]
    fn run_generates_requested_rows_and_is_repeatable() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("db");

        run(&db, "bench", "1GB", Some(5), 42).unwrap();
        let store = ColumnStore::open(&db).unwrap();
        assert_eq!(store.describe_table("bench").unwrap().next_rid, 5);
        assert!(dir_size(&db) > 0);
        drop(store);

        // Re-running drops and recreates the table rather than appending.
        run(&db, "bench", "1GB", Some(3), 42).unwrap();
        let store = ColumnStore::open(&db).unwrap();
        assert_eq!(store.describe_table("bench").unwrap().next_rid, 3);
    }

    #[test]
    fn run_derives_row_count_from_size() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path().join("db");
        // 1 MB target / ~314 est bytes per row.
        run(&db, "bench", "1MB", None, 7).unwrap();
        let store = ColumnStore::open(&db).unwrap();
        let n = store.describe_table("bench").unwrap().next_rid;
        assert_eq!(n, 1_000_000 / EST_BYTES_PER_ROW);
        assert!(n >= 1);
    }
}
