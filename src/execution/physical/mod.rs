//! The physical layer: everything that deals with the `PhysicalPlan`.
//!
//! Where the logical layer says *what* a query computes, this layer says *how*:
//! a tree of physical operators (`...Exec` nodes) that the executor drives
//! against storage. Operators pass column-major batches to one another through
//! a lazy [`BatchStream`], so `LimitExec` can stop early and `TableScanExec` can
//! skip row groups without the rest of the pipeline running.

mod executor;
mod expr;
mod lower;
mod plan;
mod prune;
mod result;

use crate::error::Error;
use crate::storage::ColumnBatch;

pub use executor::execute;
pub use lower::lower;
pub use plan::PhysicalPlan;
pub use result::QueryResult;

/// A pull-based stream of column-major batches flowing up the operator tree.
/// Mirrors storage's `BatchIter`, and ties its lifetime to the `&Storage`
/// borrow the pipeline reads through.
pub type BatchStream<'a> = Box<dyn Iterator<Item = Result<ColumnBatch, Error>> + 'a>;
