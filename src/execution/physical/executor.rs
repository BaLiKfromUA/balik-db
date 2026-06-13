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
        PhysicalPlan::ProjectionExec { .. } => Err(not_implemented("ProjectionExec")),
        PhysicalPlan::SortExec { .. } => Err(not_implemented("SortExec")),
        PhysicalPlan::LimitExec { .. } => Err(not_implemented("LimitExec")),
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
        | PhysicalPlan::LimitExec { input, .. } => output_columns(input),
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
                .filter_map(|(v, &keep)| keep.then(|| v.clone()))
                .collect()
        })
        .collect();
    ColumnBatch {
        names: batch.names.clone(),
        columns,
    }
}

fn not_implemented(op: &str) -> Error {
    Error::other(format!("{op} is not yet implemented"))
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
}
