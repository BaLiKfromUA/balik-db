//! Top-K: fuse a `Limit` sitting directly over a `Sort` into one `TopK`.
//!
//! `ORDER BY ... LIMIT n` does not need a full sort followed by a separate
//! truncation — it only needs the `n` smallest or largest rows. Merging the two
//! operators into a single node lets execution keep just `n` rows as it scans
//! instead of materializing and ordering the whole input.
//!
//! The fusion only fires when the `Limit`'s direct child is a `Sort` (the shape
//! `ORDER BY ... LIMIT` produces). A `Sort` with no `Limit`, or a `Limit` with
//! no `Sort`, is left untouched.

use super::LogicalPlan;

/// Rewrite `plan`, fusing every `Limit`-over-`Sort` pair into a `TopK`.
pub(super) fn apply(plan: LogicalPlan) -> LogicalPlan {
    match plan {
        LogicalPlan::Limit { count, input } => match apply(*input) {
            LogicalPlan::Sort {
                column,
                descending,
                input,
            } => LogicalPlan::TopK {
                column,
                descending,
                count,
                input,
            },
            other => LogicalPlan::Limit {
                count,
                input: Box::new(other),
            },
        },
        LogicalPlan::Sort {
            column,
            descending,
            input,
        } => LogicalPlan::Sort {
            column,
            descending,
            input: Box::new(apply(*input)),
        },
        LogicalPlan::Projection { columns, input } => LogicalPlan::Projection {
            columns,
            input: Box::new(apply(*input)),
        },
        LogicalPlan::Filter { predicate, input } => LogicalPlan::Filter {
            predicate,
            input: Box::new(apply(*input)),
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
            input: Box::new(apply(*input)),
        },
        leaf @ (LogicalPlan::Scan { .. }
        | LogicalPlan::CreateTable { .. }
        | LogicalPlan::Insert { .. }
        | LogicalPlan::DropTable { .. }
        | LogicalPlan::ShowTables
        | LogicalPlan::Describe { .. }
        | LogicalPlan::Delete { .. }
        | LogicalPlan::Update { .. }) => leaf,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::test_support::seeded_store;
    use crate::parser;
    use crate::storage::column_store::ColumnStore;

    fn fused(query: &str, store: &ColumnStore) -> LogicalPlan {
        let stmt = parser::parse(query).unwrap();
        let plan = crate::execution::plan(&stmt, store).unwrap();
        apply(plan)
    }

    #[test]
    fn sort_then_limit_fuse_into_topk() {
        let (_tmp, store) = seeded_store();
        let plan = fused("SELECT id FROM users ORDER BY name LIMIT 10", &store);
        assert_eq!(
            plan.to_string(),
            "Projection [id]\n  TopK [name] 10\n    Scan users"
        );
    }

    #[test]
    fn topk_keeps_sort_direction() {
        let (_tmp, store) = seeded_store();
        let plan = fused("SELECT id FROM users ORDER BY age DESC LIMIT 5", &store);
        assert_eq!(
            plan.to_string(),
            "Projection [id]\n  TopK [age DESC] 5\n    Scan users"
        );
    }

    #[test]
    fn sort_without_limit_is_left_alone() {
        let (_tmp, store) = seeded_store();
        let plan = fused("SELECT id FROM users ORDER BY name", &store);
        assert_eq!(
            plan.to_string(),
            "Projection [id]\n  Sort [name]\n    Scan users"
        );
    }

    #[test]
    fn limit_without_sort_is_left_alone() {
        let (_tmp, store) = seeded_store();
        let plan = fused("SELECT id FROM users LIMIT 5", &store);
        assert_eq!(
            plan.to_string(),
            "Projection [id]\n  Limit 5\n    Scan users"
        );
    }
}
