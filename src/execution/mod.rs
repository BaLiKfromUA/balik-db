//! Query planning and execution.
//!
//! `logical` turns a parsed AST into a [`LogicalPlan`] — the logical shape of a
//! query, validated against the catalog but not yet run — and rewrites it into
//! an equivalent, cheaper plan via its `optimizer`. `physical` lowers that plan
//! into a tree of physical operators and executes them against storage to
//! produce a [`QueryResult`].

mod logical;
mod physical;
#[cfg(test)]
mod test_support;

pub use logical::{LogicalPlan, optimize, plan};
pub use physical::{PhysicalPlan, QueryResult, execute, lower};
