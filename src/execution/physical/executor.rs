//! The executor: walk a [`PhysicalPlan`] against storage and produce a
//! [`QueryResult`].
//!
//! Two entry points cooperate. [`execute`] handles the root: DDL/DML statements
//! run for their effect, while a relational (SELECT) plan has its output column
//! names derived from the plan shape and its batch stream drained into rows.
//! [`execute_stream`] builds that lazy, pull-based stream for the relational
//! operators, recursing into each node's input.
//!
//! The relational operators are filled in operator by operator; until each
//! lands, its arm returns a plain "not yet implemented" error rather than
//! panicking, so partial builds stay runnable.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::catalog::schema::{Column, ColumnType, Schema};
use crate::catalog::tables::TableOptions;
use crate::error::Error;
use crate::parser::ast::{ColumnDef, DataType, Literal};
use crate::storage::{ColumnBatch, Record, Storage, Value};

use super::BatchStream;
use super::expr;
use super::plan::PhysicalPlan;
use super::result::{QueryResult, collect_rows};

/// Execute `plan` against `store`, returning its result. DDL/DML mutate storage
/// and report an effect; a relational plan is drained into rows.
pub fn execute(plan: &PhysicalPlan, store: &mut dyn Storage) -> Result<QueryResult, Error> {
    match plan {
        PhysicalPlan::CreateTableExec { table, columns } => {
            exec_create_table(table, columns, store)
        }
        PhysicalPlan::InsertExec { table, values } => exec_insert(table, values, store),
        _ => {
            let names = output_columns(plan)?;
            let stream = execute_stream(plan, &*store)?;
            collect_rows(names, stream)
        }
    }
}

/// Build the lazy batch stream for a relational operator. Borrows `store` for
/// as long as the stream lives, so the whole pipeline reads through one borrow.
pub(super) fn execute_stream<'a>(
    plan: &'a PhysicalPlan,
    store: &'a dyn Storage,
) -> Result<BatchStream<'a>, Error> {
    match plan {
        PhysicalPlan::TableScanExec {
            table,
            projection,
            prune,
        } => {
            let handle = store.open_table(table)?;
            // `scan_batches` returns a fully owned iterator, so the local handle
            // can drop here; the stream reads through the `store` borrow alone.
            store.scan_batches(&handle, projection.as_deref(), prune)
        }
        PhysicalPlan::FilterExec { predicate, input } => {
            let source = execute_stream(input, store)?;
            // Evaluate the predicate per batch into a selection mask and emit a
            // compacted batch keeping only the rows that pass.
            Ok(Box::new(source.map(move |batch| {
                let batch = batch?;
                let mask = expr::evaluate_predicate(predicate, &batch)?;
                Ok(filter_batch(&batch, &mask))
            })))
        }
        PhysicalPlan::ProjectionExec { columns, input } => {
            let source = execute_stream(input, store)?;
            Ok(Box::new(
                source.map(move |batch| project_batch(&batch?, columns)),
            ))
        }
        PhysicalPlan::SortExec {
            column,
            descending,
            input,
        } => {
            // Sorting is blocking: drain the whole input, order it, then emit a
            // single batch. An empty input yields no batch.
            let source = execute_stream(input, store)?;
            match sort_batches(source, column, *descending)? {
                Some(batch) => Ok(Box::new(std::iter::once(Ok(batch)))),
                None => Ok(Box::new(std::iter::empty())),
            }
        }
        PhysicalPlan::LimitExec { count, input } => {
            let mut source = execute_stream(input, store)?;
            let mut remaining = usize::try_from(*count).unwrap_or(usize::MAX);
            // Stay lazy: once the budget is spent, stop *without* pulling another
            // batch — so a scan need not decode row groups the limit never reaches.
            Ok(Box::new(std::iter::from_fn(move || {
                if remaining == 0 {
                    return None;
                }
                match source.next()? {
                    Err(e) => Some(Err(e)),
                    Ok(batch) => {
                        let take = remaining.min(batch.num_rows());
                        remaining -= take;
                        Some(Ok(truncate_batch(batch, take)))
                    }
                }
            })))
        }
        PhysicalPlan::TopKExec {
            column,
            descending,
            count,
            input,
        } => {
            // Blocking, like Sort, but keeps only the best `count` rows in a
            // bounded heap rather than sorting everything.
            let source = execute_stream(input, store)?;
            match top_k(source, column, *descending, *count)? {
                Some(batch) => Ok(Box::new(std::iter::once(Ok(batch)))),
                None => Ok(Box::new(std::iter::empty())),
            }
        }
        PhysicalPlan::CreateTableExec { .. } | PhysicalPlan::InsertExec { .. } => Err(
            Error::other("DDL/DML operator cannot appear inside a query pipeline"),
        ),
    }
}

/// The column names a relational plan produces, in order, derived from the plan
/// shape alone so an empty result still has a header. A `Projection` fixes the
/// names; the row-shaping operators above it pass their input's names through;
/// a scan carries its resolved projection.
fn output_columns(plan: &PhysicalPlan) -> Result<Vec<String>, Error> {
    match plan {
        PhysicalPlan::ProjectionExec { columns, .. } => Ok(columns.clone()),
        PhysicalPlan::FilterExec { input, .. }
        | PhysicalPlan::SortExec { input, .. }
        | PhysicalPlan::LimitExec { input, .. }
        | PhysicalPlan::TopKExec { input, .. } => output_columns(input),
        PhysicalPlan::TableScanExec {
            projection: Some(cols),
            ..
        } => Ok(cols.clone()),
        PhysicalPlan::TableScanExec {
            projection: None,
            table,
            ..
        } => Err(Error::other(format!(
            "scan of '{table}' has no resolved output columns"
        ))),
        PhysicalPlan::CreateTableExec { .. } | PhysicalPlan::InsertExec { .. } => {
            Err(Error::other("statement produces no row output"))
        }
    }
}

/// Run a CREATE TABLE: build the catalog schema from the parsed column
/// definitions and register the table. Storage creates its on-disk structures
/// and persists the schema, so the table survives a restart.
fn exec_create_table(
    table: &str,
    columns: &[ColumnDef],
    store: &mut dyn Storage,
) -> Result<QueryResult, Error> {
    let schema = Schema {
        columns: columns
            .iter()
            .map(|c| Column {
                name: c.name.clone(),
                ty: column_type(c.ty),
                nullable: c.nullable,
            })
            .collect(),
    };
    let id = store.create_table(table, schema, TableOptions::default())?;
    Ok(QueryResult::Affected(format!(
        "Created table '{table}' (id={id})"
    )))
}

fn column_type(ty: DataType) -> ColumnType {
    match ty {
        DataType::Int => ColumnType::Int,
        DataType::Text => ColumnType::Text,
    }
}

/// Run an INSERT: open the table, turn the parsed literals into a storage
/// record, and append it. The binder has already checked arity and types, and
/// storage persists the row so it survives a restart.
fn exec_insert(
    table: &str,
    values: &[Literal],
    store: &mut dyn Storage,
) -> Result<QueryResult, Error> {
    let handle = store.open_table(table)?;
    let record = Record {
        values: values.iter().map(literal_to_value).collect(),
    };
    let rid = store.insert(&handle, record)?;
    Ok(QueryResult::Affected(format!(
        "Inserted into '{table}' as rid {}",
        rid.0
    )))
}

fn literal_to_value(lit: &Literal) -> Value {
    match lit {
        Literal::Int(n) => Value::Int(*n),
        Literal::Text(s) => Value::Text(s.clone()),
        Literal::Null => Value::Null,
    }
}

/// Keep only the rows of `batch` whose mask entry is `true`, column by column.
/// The result preserves the batch's column names and order; an all-false mask
/// yields an empty (zero-row) batch.
fn filter_batch(batch: &ColumnBatch, mask: &[bool]) -> ColumnBatch {
    let columns = batch
        .columns
        .iter()
        .map(|col| {
            col.iter()
                .zip(mask)
                .filter(|&(_, &keep)| keep)
                .map(|(v, _)| v.clone())
                .collect()
        })
        .collect();
    ColumnBatch {
        names: batch.names.clone(),
        columns,
    }
}

/// Select `columns` from `batch`, in the given order, producing a new batch
/// whose names are exactly `columns`. Errors if a requested column is not
/// present in the input.
fn project_batch(batch: &ColumnBatch, columns: &[String]) -> Result<ColumnBatch, Error> {
    let mut projected = Vec::with_capacity(columns.len());
    for name in columns {
        let idx = batch.names.iter().position(|n| n == name).ok_or_else(|| {
            Error::other(format!(
                "projection references column '{name}' not present in the input"
            ))
        })?;
        projected.push(batch.columns[idx].clone());
    }
    Ok(ColumnBatch {
        names: columns.to_vec(),
        columns: projected,
    })
}

/// Drain `source`, concatenate its batches, and sort all rows by `column`.
/// Returns a single ordered batch, or `None` if the input produced no batch.
///
/// The sort column must be present in the input — that is, among the selected
/// columns, since the projection sits below the sort. Ordering is stable, so
/// rows with equal keys keep their input order.
fn sort_batches(
    source: BatchStream<'_>,
    column: &str,
    descending: bool,
) -> Result<Option<ColumnBatch>, Error> {
    let mut names: Option<Vec<String>> = None;
    let mut columns: Vec<Vec<Value>> = Vec::new();
    for batch in source {
        let batch = batch?;
        if names.is_none() {
            names = Some(batch.names);
            columns = batch.columns;
        } else {
            for (acc, col) in columns.iter_mut().zip(batch.columns) {
                acc.extend(col);
            }
        }
    }
    let Some(names) = names else {
        return Ok(None);
    };

    let key = names.iter().position(|n| n == column).ok_or_else(|| {
        Error::other(format!(
            "ORDER BY column '{column}' is not available; it must be one of the selected columns"
        ))
    })?;

    let mut order: Vec<usize> = (0..columns[key].len()).collect();
    order.sort_by(|&i, &j| {
        let ord = order_values(&columns[key][i], &columns[key][j]);
        if descending { ord.reverse() } else { ord }
    });

    let sorted = columns
        .iter()
        .map(|col| order.iter().map(|&i| col[i].clone()).collect())
        .collect();
    Ok(Some(ColumnBatch {
        names,
        columns: sorted,
    }))
}

/// One candidate row in the top-K heap, ordered by *output position*: an item
/// that should appear earlier in the result compares `Less`. Built on the shared
/// [`order_values`], with the sort direction folded in so the heap logic is
/// direction-agnostic.
struct HeapItem {
    key: Value,
    descending: bool,
    row: Vec<Value>,
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        let ord = order_values(&self.key, &other.key);
        if self.descending { ord.reverse() } else { ord }
    }
}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for HeapItem {}

/// Drain `source` and keep the `count` rows that come first under the ordering,
/// using a bounded max-heap: the heap holds at most `count` items, and whenever
/// a new row beats the current worst-kept one (the heap's max), it replaces it.
/// This is O(rows · log count) time and O(count) memory — never a full sort.
///
/// The key column must be present in the input (a selected column), as the
/// projection sits below this operator. Returns `None` if the input was empty.
fn top_k(
    source: BatchStream<'_>,
    column: &str,
    descending: bool,
    count: u64,
) -> Result<Option<ColumnBatch>, Error> {
    let k = usize::try_from(count).unwrap_or(usize::MAX);
    let mut names: Option<Vec<String>> = None;
    let mut key = 0;
    let mut heap: BinaryHeap<HeapItem> = BinaryHeap::new();

    for batch in source {
        let batch = batch?;
        if names.is_none() {
            key = batch.names.iter().position(|n| n == column).ok_or_else(|| {
                Error::other(format!(
                    "ORDER BY column '{column}' is not available; it must be one of the selected columns"
                ))
            })?;
            names = Some(batch.names.clone());
        }
        for r in 0..batch.num_rows() {
            if k == 0 {
                break;
            }
            let item = HeapItem {
                key: batch.columns[key][r].clone(),
                descending,
                row: batch.columns.iter().map(|c| c[r].clone()).collect(),
            };
            if heap.len() < k {
                heap.push(item);
            } else if item < *heap.peek().expect("heap is full, so non-empty") {
                heap.pop();
                heap.push(item);
            }
        }
    }

    let Some(names) = names else {
        return Ok(None);
    };

    // `into_sorted_vec` yields ascending by `Ord`, i.e. output order.
    let mut columns: Vec<Vec<Value>> = vec![Vec::new(); names.len()];
    for item in heap.into_sorted_vec() {
        for (col, value) in columns.iter_mut().zip(item.row) {
            col.push(value);
        }
    }
    Ok(Some(ColumnBatch { names, columns }))
}

/// Total order over values for sorting. NULLs sort before all non-null values
/// (so they come first ascending, last descending). The mixed-type arms are
/// unreachable for a single typed column but keep the ordering total.
fn order_values(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        (Value::Int(_), Value::Text(_)) => Ordering::Less,
        (Value::Text(_), Value::Int(_)) => Ordering::Greater,
    }
}

/// Keep the first `n` rows of `batch`, consuming it in place. `n` at or above
/// the row count returns the batch unchanged.
fn truncate_batch(mut batch: ColumnBatch, n: usize) -> ColumnBatch {
    if n < batch.num_rows() {
        for col in &mut batch.columns {
            col.truncate(n);
        }
    }
    batch
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::metadata;
    use crate::parser::ast::{CompareOp, Expr, LogicalOp};
    use crate::storage::column_store::ColumnStore;
    use crate::storage::{ColumnBatch, ScanCompare, ScanPredicate};
    use tempfile::TempDir;

    fn scan(projection: Option<Vec<String>>) -> PhysicalPlan {
        PhysicalPlan::TableScanExec {
            table: "users".into(),
            projection,
            prune: Vec::new(),
        }
    }

    #[test]
    fn output_columns_uses_projection_names() {
        let plan = PhysicalPlan::ProjectionExec {
            columns: vec!["id".into(), "name".into()],
            input: Box::new(scan(None)),
        };
        assert_eq!(output_columns(&plan).unwrap(), vec!["id", "name"]);
    }

    #[test]
    fn output_columns_passes_through_limit_sort_filter() {
        let plan = PhysicalPlan::LimitExec {
            count: 5,
            input: Box::new(PhysicalPlan::SortExec {
                column: "name".into(),
                descending: false,
                input: Box::new(PhysicalPlan::ProjectionExec {
                    columns: vec!["name".into()],
                    input: Box::new(scan(None)),
                }),
            }),
        };
        assert_eq!(output_columns(&plan).unwrap(), vec!["name"]);
    }

    #[test]
    fn output_columns_uses_resolved_scan_projection() {
        assert_eq!(
            output_columns(&scan(Some(vec!["id".into()]))).unwrap(),
            vec!["id"]
        );
    }

    #[test]
    fn output_columns_rejects_ddl() {
        let plan = PhysicalPlan::InsertExec {
            table: "users".into(),
            values: vec![],
        };
        assert!(output_columns(&plan).is_err());
    }

    #[test]
    fn create_table_exec_registers_table_in_catalog() {
        let (_tmp, mut store) = crate::execution::test_support::seeded_store();
        let plan = PhysicalPlan::CreateTableExec {
            table: "products".into(),
            columns: vec![
                ColumnDef {
                    name: "sku".into(),
                    ty: DataType::Int,
                    nullable: false,
                },
                ColumnDef {
                    name: "label".into(),
                    ty: DataType::Text,
                    nullable: true,
                },
            ],
        };

        let result = execute(&plan, &mut store).unwrap();
        assert!(matches!(result, QueryResult::Affected(_)));

        assert!(
            store
                .list_tables()
                .unwrap()
                .contains(&"products".to_string())
        );
        let desc = store.describe_table("products").unwrap();
        assert_eq!(desc.schema.columns.len(), 2);
        assert_eq!(desc.schema.columns[0].name, "sku");
        assert!(!desc.schema.columns[0].nullable);
        assert!(desc.schema.columns[1].nullable);
    }

    #[test]
    fn create_table_exec_rejects_duplicate_table() {
        let (_tmp, mut store) = crate::execution::test_support::seeded_store();
        let plan = PhysicalPlan::CreateTableExec {
            table: "users".into(),
            columns: vec![ColumnDef {
                name: "id".into(),
                ty: DataType::Int,
                nullable: false,
            }],
        };
        assert!(execute(&plan, &mut store).is_err());
    }

    fn insert(table: &str, values: Vec<Literal>) -> PhysicalPlan {
        PhysicalPlan::InsertExec {
            table: table.into(),
            values,
        }
    }

    #[test]
    fn insert_exec_appends_row_with_int_text_and_null() {
        let (_tmp, mut store) = crate::execution::test_support::seeded_store();

        let row = insert(
            "users",
            vec![
                Literal::Int(1),
                Literal::Text("Alice".into()),
                Literal::Int(20),
            ],
        );
        assert!(matches!(
            execute(&row, &mut store).unwrap(),
            QueryResult::Affected(_)
        ));
        // NULL into the nullable name/age columns.
        let nulls = insert("users", vec![Literal::Int(2), Literal::Null, Literal::Null]);
        execute(&nulls, &mut store).unwrap();

        let handle = store.open_table("users").unwrap();
        let mut rows: Vec<_> = store
            .scan(&handle)
            .unwrap()
            .map(|r| r.unwrap().1.values)
            .collect();
        rows.sort_by_key(|v| match v[0] {
            Value::Int(n) => n,
            _ => 0,
        });
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Text("Alice".into()), Value::Int(20)],
                vec![Value::Int(2), Value::Null, Value::Null],
            ]
        );
    }

    #[test]
    fn insert_exec_rejects_unknown_table() {
        let (_tmp, mut store) = crate::execution::test_support::seeded_store();
        let plan = insert("ghosts", vec![Literal::Int(1)]);
        assert!(execute(&plan, &mut store).is_err());
    }

    /// A `t(id INT NOT NULL, label TEXT)` store seeded with `rows` (each an id
    /// and an optional label). `row_group_size` of `Some(n)` packs `n` rows per
    /// group — use a small value to exercise whole-group skipping; `None` takes
    /// the default.
    fn store_t(
        rows: &[(i64, Option<&str>)],
        row_group_size: Option<u32>,
    ) -> (TempDir, ColumnStore) {
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
                    name: "label".into(),
                    ty: ColumnType::Text,
                    nullable: true,
                },
            ],
        };
        store
            .create_table("t", schema, TableOptions { row_group_size })
            .unwrap();
        let handle = store.open_table("t").unwrap();
        for &(id, label) in rows {
            let label = label.map_or(Value::Null, |s| Value::Text(s.into()));
            store
                .insert(
                    &handle,
                    Record {
                        values: vec![Value::Int(id), label],
                    },
                )
                .unwrap();
        }
        (tmp, store)
    }

    /// Ids packed two per row group (labels unused), for group-skipping tests.
    fn store_with_groups(ids: &[i64]) -> (TempDir, ColumnStore) {
        let rows: Vec<_> = ids.iter().map(|&i| (i, None)).collect();
        store_t(&rows, Some(2))
    }

    fn scan_t(projection: Option<Vec<String>>, prune: Vec<ScanPredicate>) -> PhysicalPlan {
        PhysicalPlan::TableScanExec {
            table: "t".into(),
            projection,
            prune,
        }
    }

    fn run_scan(plan: &PhysicalPlan, store: &dyn Storage) -> Vec<ColumnBatch> {
        execute_stream(plan, store)
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn scanned_ids(batches: &[ColumnBatch]) -> Vec<i64> {
        batches
            .iter()
            .flat_map(|b| {
                b.columns[0].iter().map(|v| match v {
                    Value::Int(n) => *n,
                    other => panic!("expected int id, got {other:?}"),
                })
            })
            .collect()
    }

    #[test]
    fn table_scan_reads_every_row_in_schema_column_order() {
        let (_tmp, store) = store_with_groups(&[1, 2, 3]);
        let batches = run_scan(&scan_t(None, vec![]), &store);
        assert_eq!(
            batches[0].names,
            vec!["id".to_string(), "label".to_string()]
        );
        assert_eq!(scanned_ids(&batches), vec![1, 2, 3]);
    }

    #[test]
    fn table_scan_projection_decodes_only_requested_columns() {
        let (_tmp, store) = store_with_groups(&[1, 2]);
        let batches = run_scan(&scan_t(Some(vec!["id".into()]), vec![]), &store);
        assert_eq!(batches[0].names, vec!["id".to_string()]);
        assert_eq!(batches[0].columns.len(), 1);
    }

    #[test]
    fn table_scan_skips_groups_proven_non_matching_but_does_not_filter_rows() {
        // ids 1..=6 → groups [1,2] [3,4] [5,6]. `id > 3` prunes [1,2] (max 2),
        // keeps [3,4] and [5,6]. The kept group [3,4] still yields id=3, which
        // does not satisfy the predicate: a scan skips groups, it does not
        // filter rows — that is FilterExec's job.
        let (_tmp, store) = store_with_groups(&[1, 2, 3, 4, 5, 6]);
        let prune = vec![ScanPredicate {
            column: "id".into(),
            op: ScanCompare::Gt,
            value: 3,
        }];
        let batches = run_scan(&scan_t(Some(vec!["id".into()]), prune), &store);
        assert_eq!(scanned_ids(&batches), vec![3, 4, 5, 6]);
    }

    #[test]
    fn table_scan_skips_all_groups_when_nothing_can_match() {
        let (_tmp, store) = store_with_groups(&[1, 2, 3, 4]);
        let prune = vec![ScanPredicate {
            column: "id".into(),
            op: ScanCompare::Gt,
            value: 100,
        }];
        let batches = run_scan(&scan_t(Some(vec!["id".into()]), prune), &store);
        assert!(batches.is_empty());
    }

    fn filter(predicate: Expr, input: PhysicalPlan) -> PhysicalPlan {
        PhysicalPlan::FilterExec {
            predicate,
            input: Box::new(input),
        }
    }

    fn id_cmp(op: CompareOp, value: i64) -> Expr {
        Expr::Compare {
            left: Box::new(Expr::Column("id".into())),
            op,
            right: Box::new(Expr::Literal(Literal::Int(value))),
        }
    }

    #[test]
    fn filter_exec_drops_rows_left_in_kept_groups() {
        // Scan prunes group [1,2] but keeps [3,4] (with non-matching id=3) and
        // [5,6]; the filter then removes id=3, so only 4,5,6 survive.
        let (_tmp, store) = store_with_groups(&[1, 2, 3, 4, 5, 6]);
        let prune = vec![ScanPredicate {
            column: "id".into(),
            op: ScanCompare::Gt,
            value: 3,
        }];
        let scan = scan_t(Some(vec!["id".into()]), prune);
        let batches = run_scan(&filter(id_cmp(CompareOp::Gt, 3), scan), &store);
        assert_eq!(scanned_ids(&batches), vec![4, 5, 6]);
    }

    #[test]
    fn filter_exec_applies_and_of_two_bounds() {
        // id >= 2 AND id <= 4
        let (_tmp, store) = store_with_groups(&[1, 2, 3, 4, 5, 6]);
        let scan = scan_t(Some(vec!["id".into()]), vec![]);
        let pred = Expr::Logical {
            left: Box::new(id_cmp(CompareOp::GtEq, 2)),
            op: LogicalOp::And,
            right: Box::new(id_cmp(CompareOp::LtEq, 4)),
        };
        let batches = run_scan(&filter(pred, scan), &store);
        assert_eq!(scanned_ids(&batches), vec![2, 3, 4]);
    }

    #[test]
    fn filter_exec_eq_selects_the_exact_int() {
        let (_tmp, store) = store_with_groups(&[1, 2, 3, 4, 5, 6]);
        let scan = scan_t(Some(vec!["id".into()]), vec![]);
        let batches = run_scan(&filter(id_cmp(CompareOp::Eq, 3), scan), &store);
        assert_eq!(scanned_ids(&batches), vec![3]);
    }

    #[test]
    fn filter_exec_not_eq_excludes_the_value() {
        let (_tmp, store) = store_with_groups(&[1, 2, 3, 4]);
        let scan = scan_t(Some(vec!["id".into()]), vec![]);
        let batches = run_scan(&filter(id_cmp(CompareOp::NotEq, 3), scan), &store);
        assert_eq!(scanned_ids(&batches), vec![1, 2, 4]);
    }

    /// Explicit — possibly NULL — labels for exercising text and NULL filtering.
    fn store_with_labels(rows: &[(i64, Option<&str>)]) -> (TempDir, ColumnStore) {
        store_t(rows, None)
    }

    fn label_cmp(op: CompareOp, value: &str) -> Expr {
        Expr::Compare {
            left: Box::new(Expr::Column("label".into())),
            op,
            right: Box::new(Expr::Literal(Literal::Text(value.into()))),
        }
    }

    #[test]
    fn filter_exec_eq_on_text_column() {
        let (_tmp, store) = store_with_labels(&[(1, Some("a")), (2, None), (3, Some("b"))]);
        let scan = scan_t(Some(vec!["id".into(), "label".into()]), vec![]);
        let batches = run_scan(&filter(label_cmp(CompareOp::Eq, "a"), scan), &store);
        assert_eq!(scanned_ids(&batches), vec![1]);
    }

    #[test]
    fn filter_exec_not_eq_on_text_excludes_null_rows() {
        // `label != 'a'` keeps 'b' (id 3) but drops the NULL label (id 2):
        // a comparison against NULL is never true.
        let (_tmp, store) = store_with_labels(&[(1, Some("a")), (2, None), (3, Some("b"))]);
        let scan = scan_t(Some(vec!["id".into(), "label".into()]), vec![]);
        let batches = run_scan(&filter(label_cmp(CompareOp::NotEq, "a"), scan), &store);
        assert_eq!(scanned_ids(&batches), vec![3]);
    }

    #[test]
    fn filter_exec_eq_on_text_never_matches_null() {
        let (_tmp, store) = store_with_labels(&[(1, None), (2, Some("x"))]);
        let scan = scan_t(Some(vec!["id".into(), "label".into()]), vec![]);
        let batches = run_scan(&filter(label_cmp(CompareOp::Eq, "x"), scan), &store);
        assert_eq!(scanned_ids(&batches), vec![2]);
    }

    fn project(columns: Vec<&str>, input: PhysicalPlan) -> PhysicalPlan {
        PhysicalPlan::ProjectionExec {
            columns: columns.into_iter().map(Into::into).collect(),
            input: Box::new(input),
        }
    }

    #[test]
    fn projection_exec_selects_and_reorders_columns() {
        let (_tmp, store) = store_with_labels(&[(1, Some("a")), (2, Some("b"))]);
        let scan = scan_t(Some(vec!["id".into(), "label".into()]), vec![]);
        let batches = run_scan(&project(vec!["label", "id"], scan), &store);
        assert_eq!(
            batches[0].names,
            vec!["label".to_string(), "id".to_string()]
        );
        assert_eq!(
            batches[0].columns[0],
            vec![Value::Text("a".into()), Value::Text("b".into())]
        );
        assert_eq!(batches[0].columns[1], vec![Value::Int(1), Value::Int(2)]);
    }

    #[test]
    fn projection_exec_selects_a_subset() {
        let (_tmp, store) = store_with_labels(&[(1, Some("a")), (2, Some("b"))]);
        let scan = scan_t(Some(vec!["id".into(), "label".into()]), vec![]);
        let batches = run_scan(&project(vec!["id"], scan), &store);
        assert_eq!(batches[0].names, vec!["id".to_string()]);
        assert_eq!(batches[0].columns.len(), 1);
    }

    #[test]
    fn projection_exec_runs_after_filter() {
        let (_tmp, store) = store_with_labels(&[(1, Some("a")), (2, Some("b")), (3, Some("c"))]);
        let scan = scan_t(Some(vec!["id".into(), "label".into()]), vec![]);
        let filtered = filter(id_cmp(CompareOp::GtEq, 2), scan);
        let batches = run_scan(&project(vec!["label"], filtered), &store);
        let labels: Vec<_> = batches.iter().flat_map(|b| b.columns[0].clone()).collect();
        assert_eq!(
            labels,
            vec![Value::Text("b".into()), Value::Text("c".into())]
        );
    }

    #[test]
    fn projection_exec_unknown_column_errors() {
        let (_tmp, store) = store_with_labels(&[(1, Some("a"))]);
        let scan = scan_t(Some(vec!["id".into()]), vec![]);
        let plan = project(vec!["ghost"], scan);
        let mut stream = execute_stream(&plan, &store).unwrap();
        assert!(stream.next().unwrap().is_err());
    }

    fn sort(column: &str, descending: bool, input: PhysicalPlan) -> PhysicalPlan {
        PhysicalPlan::SortExec {
            column: column.into(),
            descending,
            input: Box::new(input),
        }
    }

    #[test]
    fn sort_exec_orders_ascending_by_int() {
        let (_tmp, store) = store_with_labels(&[(3, Some("c")), (1, Some("a")), (2, Some("b"))]);
        let scan = scan_t(Some(vec!["id".into(), "label".into()]), vec![]);
        let batches = run_scan(&sort("id", false, scan), &store);
        assert_eq!(scanned_ids(&batches), vec![1, 2, 3]);
    }

    #[test]
    fn sort_exec_orders_descending_by_int() {
        let (_tmp, store) = store_with_labels(&[(3, Some("c")), (1, Some("a")), (2, Some("b"))]);
        let scan = scan_t(Some(vec!["id".into(), "label".into()]), vec![]);
        let batches = run_scan(&sort("id", true, scan), &store);
        assert_eq!(scanned_ids(&batches), vec![3, 2, 1]);
    }

    #[test]
    fn sort_exec_orders_by_text_column() {
        // Labels order differently from ids: a(id2), b(id3), c(id1).
        let (_tmp, store) = store_with_labels(&[(1, Some("c")), (2, Some("a")), (3, Some("b"))]);
        let scan = scan_t(Some(vec!["id".into(), "label".into()]), vec![]);
        let batches = run_scan(&sort("label", false, scan), &store);
        assert_eq!(scanned_ids(&batches), vec![2, 3, 1]);
    }

    #[test]
    fn sort_exec_places_nulls_first_ascending() {
        // NULL (id2) sorts before 'a' (id3) before 'b' (id1).
        let (_tmp, store) = store_with_labels(&[(1, Some("b")), (2, None), (3, Some("a"))]);
        let scan = scan_t(Some(vec!["id".into(), "label".into()]), vec![]);
        let batches = run_scan(&sort("label", false, scan), &store);
        assert_eq!(scanned_ids(&batches), vec![2, 3, 1]);
    }

    #[test]
    fn sort_exec_errors_when_column_not_in_input() {
        // `label` was not selected, so the sort cannot see it.
        let (_tmp, store) = store_with_labels(&[(1, Some("a"))]);
        let scan = scan_t(Some(vec!["id".into()]), vec![]);
        let plan = sort("label", false, scan);
        assert!(execute_stream(&plan, &store).is_err());
    }

    fn limit(count: u64, input: PhysicalPlan) -> PhysicalPlan {
        PhysicalPlan::LimitExec {
            count,
            input: Box::new(input),
        }
    }

    #[test]
    fn limit_exec_caps_rows_below_total() {
        let (_tmp, store) = store_with_groups(&[1, 2, 3, 4, 5, 6]);
        let scan = scan_t(Some(vec!["id".into()]), vec![]);
        let batches = run_scan(&limit(2, scan), &store);
        assert_eq!(scanned_ids(&batches), vec![1, 2]);
    }

    #[test]
    fn limit_exec_above_total_returns_everything() {
        let (_tmp, store) = store_with_groups(&[1, 2, 3]);
        let scan = scan_t(Some(vec!["id".into()]), vec![]);
        let batches = run_scan(&limit(10, scan), &store);
        assert_eq!(scanned_ids(&batches), vec![1, 2, 3]);
    }

    #[test]
    fn limit_exec_zero_is_empty() {
        let (_tmp, store) = store_with_groups(&[1, 2, 3]);
        let scan = scan_t(Some(vec!["id".into()]), vec![]);
        let batches = run_scan(&limit(0, scan), &store);
        assert_eq!(scanned_ids(&batches), Vec::<i64>::new());
    }

    #[test]
    fn limit_exec_truncates_across_a_batch_boundary() {
        // Row groups of 2: [1,2] [3,4] [5,6]. LIMIT 3 takes the first group whole
        // and one row of the second.
        let (_tmp, store) = store_with_groups(&[1, 2, 3, 4, 5, 6]);
        let scan = scan_t(Some(vec!["id".into()]), vec![]);
        let batches = run_scan(&limit(3, scan), &store);
        assert_eq!(scanned_ids(&batches), vec![1, 2, 3]);
    }

    #[test]
    fn limit_exec_applies_after_filter() {
        let (_tmp, store) = store_with_groups(&[1, 2, 3, 4, 5, 6]);
        let scan = scan_t(Some(vec!["id".into()]), vec![]);
        let filtered = filter(id_cmp(CompareOp::Gt, 2), scan);
        let batches = run_scan(&limit(2, filtered), &store);
        assert_eq!(scanned_ids(&batches), vec![3, 4]);
    }

    fn topk(column: &str, descending: bool, count: u64, input: PhysicalPlan) -> PhysicalPlan {
        PhysicalPlan::TopKExec {
            column: column.into(),
            descending,
            count,
            input: Box::new(input),
        }
    }

    #[test]
    fn top_k_keeps_smallest_ascending_across_batches() {
        // Row groups of 2 → the heap must span batches: [5,3] [1,6] [2,4].
        let (_tmp, store) = store_with_groups(&[5, 3, 1, 6, 2, 4]);
        let scan = scan_t(Some(vec!["id".into()]), vec![]);
        let batches = run_scan(&topk("id", false, 3, scan), &store);
        assert_eq!(scanned_ids(&batches), vec![1, 2, 3]);
    }

    #[test]
    fn top_k_keeps_largest_descending() {
        let (_tmp, store) = store_with_groups(&[5, 3, 1, 6, 2, 4]);
        let scan = scan_t(Some(vec!["id".into()]), vec![]);
        let batches = run_scan(&topk("id", true, 2, scan), &store);
        assert_eq!(scanned_ids(&batches), vec![6, 5]);
    }

    #[test]
    fn top_k_orders_by_text_column() {
        let (_tmp, store) = store_with_labels(&[(1, Some("c")), (2, Some("a")), (3, Some("b"))]);
        let scan = scan_t(Some(vec!["id".into(), "label".into()]), vec![]);
        let batches = run_scan(&topk("label", false, 2, scan), &store);
        assert_eq!(scanned_ids(&batches), vec![2, 3]);
    }

    #[test]
    fn top_k_ascending_places_nulls_first() {
        let (_tmp, store) = store_with_labels(&[(1, Some("b")), (2, None), (3, Some("a"))]);
        let scan = scan_t(Some(vec!["id".into(), "label".into()]), vec![]);
        let batches = run_scan(&topk("label", false, 2, scan), &store);
        assert_eq!(scanned_ids(&batches), vec![2, 3]);
    }

    #[test]
    fn top_k_count_above_total_sorts_everything() {
        let (_tmp, store) = store_with_groups(&[3, 1, 2]);
        let scan = scan_t(Some(vec!["id".into()]), vec![]);
        let batches = run_scan(&topk("id", false, 10, scan), &store);
        assert_eq!(scanned_ids(&batches), vec![1, 2, 3]);
    }

    #[test]
    fn top_k_count_zero_is_empty() {
        let (_tmp, store) = store_with_groups(&[3, 1, 2]);
        let scan = scan_t(Some(vec!["id".into()]), vec![]);
        let batches = run_scan(&topk("id", false, 0, scan), &store);
        assert_eq!(scanned_ids(&batches), Vec::<i64>::new());
    }

    #[test]
    fn top_k_errors_when_column_not_in_input() {
        let (_tmp, store) = store_with_labels(&[(1, Some("a"))]);
        let scan = scan_t(Some(vec!["id".into()]), vec![]);
        let plan = topk("label", false, 1, scan);
        assert!(execute_stream(&plan, &store).is_err());
    }

    #[test]
    fn execute_select_returns_rows_with_projected_columns() {
        // The full relational path: scan → project → collected QueryResult::Rows.
        let (_tmp, mut store) = store_with_labels(&[(1, Some("a")), (2, Some("b"))]);
        let scan = scan_t(Some(vec!["id".into(), "label".into()]), vec![]);
        let plan = project(vec!["label", "id"], scan);
        let result = execute(&plan, &mut store).unwrap();
        assert_eq!(
            result,
            QueryResult::Rows {
                names: vec!["label".into(), "id".into()],
                rows: vec![
                    vec![Value::Text("a".into()), Value::Int(1)],
                    vec![Value::Text("b".into()), Value::Int(2)],
                ],
            }
        );
    }
}
