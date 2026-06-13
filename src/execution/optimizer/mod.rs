//! Logical optimizer: rewrites a [`LogicalPlan`] into an equivalent plan that
//! is cheaper to execute, without changing the rows it produces.
//!
//! Optimization is a pure `LogicalPlan -> LogicalPlan` transformation. It reads
//! no storage and no catalog — every rule is derivable from the plan tree
//! itself — so it can run anywhere a plan is available.
//!
//! Two rules run:
//! - top-K, which fuses an adjacent `Sort` and `Limit` into a single `TopK`;
//! - column pushdown, which records on each `Scan` the columns it actually
//!   needs to produce so a column store can read only those.

mod column_pushdown;
mod top_k;

use super::LogicalPlan;

/// Apply the optimization rules to `plan` and return the rewritten plan.
///
/// Top-K runs first so column pushdown sees the fused `TopK` and still collects
/// its ordering column; the rules are otherwise independent.
pub fn optimize(plan: LogicalPlan) -> LogicalPlan {
    let plan = top_k::apply(plan);
    column_pushdown::apply(plan)
}
