//! Column pushdown: record on a `Scan` the columns it must produce.
//!
//! The binder emits a `Scan` with no column list, meaning "every column". But a
//! query only ever needs the columns it projects, filters on, or sorts by. By
//! pushing that set down onto the `Scan`, a column store can read just those
//! `.col` files instead of the whole row.
//!
//! The needed set is the projected columns first (preserving their order), then
//! any columns referenced only by `Filter` or `Sort`, de-duplicated. Plans
//! without a `Scan` (`CreateTable`, `Insert`) pass through unchanged.

use super::super::planner::collect_expr_columns;
use super::LogicalPlan;

/// Stamp the scan beneath `plan` with the columns the query needs.
pub(super) fn apply(plan: LogicalPlan) -> LogicalPlan {
    let needed = needed_columns(&plan);
    match needed {
        Some(columns) => set_scan_columns(plan, columns),
        None => plan,
    }
}

/// Walk the operator chain and return the columns a scan must produce, or
/// `None` if the plan has no scan to push onto.
fn needed_columns(plan: &LogicalPlan) -> Option<Vec<String>> {
    let mut projected = Vec::new();
    let mut extra = Vec::new();
    if !collect(plan, &mut projected, &mut extra) {
        return None;
    }
    let mut needed = Vec::new();
    for column in projected.into_iter().chain(extra) {
        if !needed.contains(&column) {
            needed.push(column);
        }
    }
    Some(needed)
}

/// Gather projected columns into `projected` and filter/sort columns into
/// `extra`, returning whether a `Scan` was found at the leaf.
fn collect(plan: &LogicalPlan, projected: &mut Vec<String>, extra: &mut Vec<String>) -> bool {
    match plan {
        LogicalPlan::Scan { .. } => true,
        LogicalPlan::Projection { columns, input } => {
            projected.extend(columns.iter().cloned());
            collect(input, projected, extra)
        }
        LogicalPlan::Filter { predicate, input } => {
            collect_expr_columns(predicate, extra);
            collect(input, projected, extra)
        }
        LogicalPlan::Sort { column, input, .. } => {
            extra.push(column.clone());
            collect(input, projected, extra)
        }
        LogicalPlan::Limit { input, .. } => collect(input, projected, extra),
        LogicalPlan::CreateTable { .. } | LogicalPlan::Insert { .. } => false,
    }
}

/// Rebuild `plan`, replacing the scan's column list with `columns`.
fn set_scan_columns(plan: LogicalPlan, columns: Vec<String>) -> LogicalPlan {
    match plan {
        LogicalPlan::Scan { table, .. } => LogicalPlan::Scan {
            table,
            columns: Some(columns),
        },
        LogicalPlan::Filter { predicate, input } => LogicalPlan::Filter {
            predicate,
            input: Box::new(set_scan_columns(*input, columns)),
        },
        LogicalPlan::Projection {
            columns: projected,
            input,
        } => LogicalPlan::Projection {
            columns: projected,
            input: Box::new(set_scan_columns(*input, columns)),
        },
        LogicalPlan::Sort {
            column,
            descending,
            input,
        } => LogicalPlan::Sort {
            column,
            descending,
            input: Box::new(set_scan_columns(*input, columns)),
        },
        LogicalPlan::Limit { count, input } => LogicalPlan::Limit {
            count,
            input: Box::new(set_scan_columns(*input, columns)),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::metadata;
    use crate::catalog::schema::{Column, ColumnType, Schema};
    use crate::parser;
    use crate::storage::Storage;
    use crate::storage::column_store::ColumnStore;
    use tempfile::TempDir;

    /// A column store with a `users(id INT NOT NULL, name TEXT, age INT)` table.
    fn seeded_store() -> (TempDir, ColumnStore) {
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

    fn optimized(query: &str, store: &ColumnStore) -> LogicalPlan {
        let stmt = parser::parse(query).unwrap();
        let plan = crate::execution::plan(&stmt, store).unwrap();
        apply(plan)
    }

    #[test]
    fn scan_lists_projection_then_filter_and_sort_columns() {
        let (_tmp, store) = seeded_store();
        let plan = optimized(
            "SELECT id, name FROM users WHERE age > 18 ORDER BY name",
            &store,
        );
        assert_eq!(
            plan.to_string(),
            "Sort [name]\n  Projection [id, name]\n    Filter [age > 18]\n      Scan users [id, name, age]"
        );
    }

    #[test]
    fn select_star_scan_lists_all_columns() {
        let (_tmp, store) = seeded_store();
        let plan = optimized("SELECT * FROM users", &store);
        assert_eq!(
            plan.to_string(),
            "Projection [id, name, age]\n  Scan users [id, name, age]"
        );
    }

    #[test]
    fn duplicate_references_are_listed_once() {
        let (_tmp, store) = seeded_store();
        let plan = optimized("SELECT id FROM users WHERE id > 1 ORDER BY id", &store);
        assert_eq!(
            plan.to_string(),
            "Sort [id]\n  Projection [id]\n    Filter [id > 1]\n      Scan users [id]"
        );
    }

    #[test]
    fn create_table_passes_through_unchanged() {
        let (_tmp, store) = seeded_store();
        let plan = optimized("CREATE TABLE t (a INT, b TEXT)", &store);
        assert_eq!(plan.to_string(), "CreateTable t [a INT, b TEXT]");
    }

    #[test]
    fn insert_passes_through_unchanged() {
        let (_tmp, store) = seeded_store();
        let plan = optimized("INSERT INTO users VALUES (1, 'Alice', 20)", &store);
        assert_eq!(plan.to_string(), "Insert users [1, 'Alice', 20]");
    }
}
