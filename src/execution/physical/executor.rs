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
use crate::parser::ast::{ColumnDef, DataType};
use crate::storage::Storage;

use super::BatchStream;
use super::plan::PhysicalPlan;
use super::result::{QueryResult, collect_rows};

/// Execute `plan` against `store`, returning its result. DDL/DML mutate storage
/// and report an effect; a relational plan is drained into rows.
pub fn execute(plan: &PhysicalPlan, store: &mut dyn Storage) -> Result<QueryResult, Error> {
    match plan {
        PhysicalPlan::CreateTableExec { table, columns } => {
            exec_create_table(table, columns, store)
        }
        PhysicalPlan::InsertExec { .. } => exec_insert(plan, store),
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
    let _ = store;
    match plan {
        PhysicalPlan::TableScanExec { .. } => Err(not_implemented("TableScanExec")),
        PhysicalPlan::FilterExec { .. } => Err(not_implemented("FilterExec")),
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

fn exec_insert(_plan: &PhysicalPlan, _store: &mut dyn Storage) -> Result<QueryResult, Error> {
    Err(not_implemented("InsertExec"))
}

fn not_implemented(op: &str) -> Error {
    Error::other(format!("{op} is not yet implemented"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
