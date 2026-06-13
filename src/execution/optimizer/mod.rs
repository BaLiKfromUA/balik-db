//! Logical optimizer: rewrites a [`LogicalPlan`] into an equivalent plan that
//! is cheaper to execute, without changing the rows it produces.
//!
//! Optimization is a pure `LogicalPlan -> LogicalPlan` transformation. It reads
//! no storage and no catalog — every rule is derivable from the plan tree
//! itself — so it can run anywhere a plan is available.
//!
//! Currently one rule runs: column pushdown, which records on each `Scan` the
//! columns it actually needs to produce so a column store can read only those.

mod column_pushdown;

use super::LogicalPlan;

/// Apply the optimization rules to `plan` and return the rewritten plan.
pub fn optimize(plan: LogicalPlan) -> LogicalPlan {
    column_pushdown::apply(plan)
}
