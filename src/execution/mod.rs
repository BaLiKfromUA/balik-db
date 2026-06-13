//! Query planning and execution.
//!
//! `logical` turns a parsed AST into a [`LogicalPlan`] — the logical shape of a
//! query, validated against the catalog but not yet run — and rewrites it into
//! an equivalent, cheaper plan via its `optimizer`. Execution (walking a plan
//! against storage to produce rows) will join it here in a later stage.

mod logical;
#[cfg(test)]
mod test_support;

pub use logical::{LogicalPlan, optimize, plan};
