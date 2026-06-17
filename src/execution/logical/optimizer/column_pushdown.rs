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

use std::collections::HashSet;

use super::super::collect_expr_columns;
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
    if !collect_required_columns(plan, &mut projected, &mut extra) {
        return None;
    }
    // Projected columns first, then filter/sort-only columns, each kept once.
    let mut seen = HashSet::new();
    let needed = projected
        .into_iter()
        .chain(extra)
        .filter(|column| seen.insert(column.clone()))
        .collect();
    Some(needed)
}

/// Gather projected columns into `projected` and filter/sort columns into
/// `extra`, returning whether a `Scan` was found at the leaf.
fn collect_required_columns(
    plan: &LogicalPlan,
    projected: &mut Vec<String>,
    extra: &mut Vec<String>,
) -> bool {
    match plan {
        LogicalPlan::Scan { .. } => true,
        LogicalPlan::Projection { columns, input } => {
            projected.extend(columns.iter().cloned());
            collect_required_columns(input, projected, extra)
        }
        LogicalPlan::Filter { predicate, input } => {
            collect_expr_columns(predicate, extra);
            collect_required_columns(input, projected, extra)
        }
        LogicalPlan::Sort { column, input, .. } | LogicalPlan::TopK { column, input, .. } => {
            extra.push(column.clone());
            collect_required_columns(input, projected, extra)
        }
        LogicalPlan::Limit { input, .. } => collect_required_columns(input, projected, extra),
        LogicalPlan::CreateTable { .. }
        | LogicalPlan::Insert { .. }
        | LogicalPlan::DropTable { .. }
        | LogicalPlan::ShowTables => false,
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
        LogicalPlan::TopK {
            column,
            descending,
            count,
            input,
        } => LogicalPlan::TopK {
            column,
            descending,
            count,
            input: Box::new(set_scan_columns(*input, columns)),
        },
        // No scan to stamp; these are standalone roots. Listed explicitly so a
        // new operator above a `Scan` is a compile error here, not a silently
        // dropped recursion that leaves the scan un-pruned.
        dml @ (LogicalPlan::CreateTable { .. }
        | LogicalPlan::Insert { .. }
        | LogicalPlan::DropTable { .. }
        | LogicalPlan::ShowTables) => dml,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::test_support::seeded_store;
    use crate::parser;
    use crate::storage::column_store::ColumnStore;

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
            "Projection [id, name]\n  Sort [name]\n    Filter [age > 18]\n      Scan users [id, name, age]"
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
            "Projection [id]\n  Sort [id]\n    Filter [id > 1]\n      Scan users [id]"
        );
    }

    #[test]
    fn topk_ordering_column_is_pushed_onto_scan() {
        // The full pipeline fuses the Sort+Limit into a TopK before column
        // pushdown runs, so the ordering column must still reach the scan.
        let (_tmp, store) = seeded_store();
        let stmt = parser::parse("SELECT id FROM users ORDER BY name LIMIT 10").unwrap();
        let plan = crate::execution::optimize(crate::execution::plan(&stmt, &store).unwrap());
        assert_eq!(
            plan.to_string(),
            "Projection [id]\n  TopK [name] 10\n    Scan users [id, name]"
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
