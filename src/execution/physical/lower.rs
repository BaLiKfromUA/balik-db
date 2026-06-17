//! Lowering: `LogicalPlan` → `PhysicalPlan`.
//!
//! Each logical node maps to its physical operator. Two things happen on the way
//! down beyond a straight rename:
//!
//! - the WHERE predicate of a `Filter` is mined for `column <op> integer`
//!   conjuncts, which ride down onto the `TableScanExec` beneath it as row-group
//!   pruning hints;
//! - the column list a column-pushdown rewrite left on a `Scan` is carried onto
//!   the `TableScanExec` as its projection.
//!
//! Whether the input was optimized is the caller's choice: `Sort`/`Limit` lower
//! to `SortExec`/`LimitExec`, and the optimizer's fused `TopK` lowers to the
//! bounded-heap `TopKExec` — lowering handles whichever it is given.

use crate::execution::LogicalPlan;
use crate::storage::ScanPredicate;

use super::plan::PhysicalPlan;
use super::prune::extract_scan_predicates;

/// Lower a logical plan into a physical plan.
pub fn lower(plan: &LogicalPlan) -> PhysicalPlan {
    lower_node(plan, &[])
}

/// `prune` carries the hints extracted from an enclosing `Filter` down to the
/// scan beneath it; it is empty everywhere else.
fn lower_node(plan: &LogicalPlan, prune: &[ScanPredicate]) -> PhysicalPlan {
    match plan {
        LogicalPlan::CreateTable { table, columns } => PhysicalPlan::CreateTableExec {
            table: table.clone(),
            columns: columns.clone(),
        },
        LogicalPlan::Insert { table, values } => PhysicalPlan::InsertExec {
            table: table.clone(),
            values: values.clone(),
        },
        LogicalPlan::DropTable { table } => PhysicalPlan::DropTableExec {
            table: table.clone(),
        },
        LogicalPlan::Scan { table, columns } => PhysicalPlan::TableScanExec {
            table: table.clone(),
            projection: columns.clone(),
            prune: prune.to_vec(),
        },
        LogicalPlan::Filter { predicate, input } => {
            // The scan beneath this filter can skip row groups using the
            // predicate's INT conjuncts.
            let hints = extract_scan_predicates(predicate);
            PhysicalPlan::FilterExec {
                predicate: predicate.clone(),
                input: Box::new(lower_node(input, &hints)),
            }
        }
        LogicalPlan::Projection { columns, input } => PhysicalPlan::ProjectionExec {
            columns: columns.clone(),
            input: Box::new(lower_node(input, prune)),
        },
        LogicalPlan::Sort {
            column,
            descending,
            input,
        } => PhysicalPlan::SortExec {
            column: column.clone(),
            descending: *descending,
            input: Box::new(lower_node(input, prune)),
        },
        LogicalPlan::Limit { count, input } => PhysicalPlan::LimitExec {
            count: *count,
            input: Box::new(lower_node(input, prune)),
        },
        LogicalPlan::TopK {
            column,
            descending,
            count,
            input,
        } => PhysicalPlan::TopKExec {
            column: column.clone(),
            descending: *descending,
            count: *count,
            input: Box::new(lower_node(input, prune)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::test_support::seeded_store;
    use crate::execution::{optimize, plan};
    use crate::parser;

    /// Parse, bind against the seeded `users` table, optionally optimize, and
    /// lower — returning the rendered physical plan.
    fn lowered(sql: &str, optimize_it: bool) -> String {
        let (_tmp, store) = seeded_store();
        let stmt = parser::parse(sql).unwrap();
        let mut logical = plan(&stmt, &store).unwrap();
        if optimize_it {
            logical = optimize(logical);
        }
        lower(&logical).to_string()
    }

    #[test]
    fn optimized_select_chain_lowers_to_topk_with_pushed_columns_and_prune() {
        let physical = lowered(
            "SELECT id, name FROM users WHERE age > 18 ORDER BY name LIMIT 10",
            true,
        );
        assert_eq!(
            physical,
            "ProjectionExec [id, name]\n  \
               TopKExec [name] 10\n    \
                 FilterExec [age > 18]\n      \
                   TableScanExec users [id, name, age] prune=[age > 18]"
        );
    }

    #[test]
    fn unoptimized_select_chain_lowers_to_sort_and_limit_and_still_prunes() {
        // Without optimization there is no TopK fusion and no column pushdown,
        // but the filter predicate is still mined for pruning hints.
        let physical = lowered(
            "SELECT id, name FROM users WHERE age > 18 ORDER BY name LIMIT 10",
            false,
        );
        assert_eq!(
            physical,
            "ProjectionExec [id, name]\n  \
               LimitExec 10\n    \
                 SortExec [name]\n      \
                   FilterExec [age > 18]\n        \
                     TableScanExec users prune=[age > 18]"
        );
    }

    #[test]
    fn select_star_lowers_to_projection_over_scan() {
        let physical = lowered("SELECT * FROM users", false);
        assert_eq!(
            physical,
            "ProjectionExec [id, name, age]\n  TableScanExec users"
        );
    }

    #[test]
    fn create_table_lowers_to_create_table_exec() {
        let physical = lowered("CREATE TABLE t (a INT, b TEXT)", false);
        assert_eq!(physical, "CreateTableExec t [a INT, b TEXT]");
    }

    #[test]
    fn insert_lowers_to_insert_exec() {
        let physical = lowered("INSERT INTO users VALUES (1, 'Alice', 20)", false);
        assert_eq!(physical, "InsertExec users [1, 'Alice', 20]");
    }

    #[test]
    fn drop_table_lowers_to_drop_table_exec() {
        let physical = lowered("DROP TABLE users", false);
        assert_eq!(physical, "DropTableExec users");
    }
}
