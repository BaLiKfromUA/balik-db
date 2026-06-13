//! The logical layer: everything that deals with the `LogicalPlan`.
//!
//! The parser produces an AST that mirrors the query's syntax. The `binder`
//! turns it into a tree of logical operators that says *what* to do — scan a
//! table, filter it, project columns, sort, limit — and validates it against
//! the catalog. The `optimizer` then rewrites that plan into an equivalent,
//! cheaper one. Neither executes anything nor reads row data.

mod binder;
mod optimizer;
mod plan;

pub use optimizer::optimize;
pub use plan::LogicalPlan;
pub(crate) use plan::collect_expr_columns;

use crate::error::Error;
use crate::parser::ast::Statement;
use crate::storage::Storage;

/// Build a validated logical plan for `stmt`, using `storage`'s catalog to
/// resolve and check table and column references.
pub fn plan(stmt: &Statement, storage: &dyn Storage) -> Result<LogicalPlan, Error> {
    binder::bind(stmt, storage)
}
