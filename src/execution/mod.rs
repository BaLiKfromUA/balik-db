//! Query planning and execution.
//!
//! `planner` turns a parsed AST into a [`LogicalPlan`] — the logical shape of a
//! query, validated against the catalog but not yet run. `optimizer` rewrites a
//! plan into an equivalent, cheaper one. Execution (walking a plan against
//! storage to produce rows) will join it here in a later stage.

mod optimizer;
mod planner;

pub use optimizer::optimize;
pub use planner::{LogicalPlan, plan};
