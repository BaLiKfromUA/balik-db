//! The physical plan: a tree of physical operators (the `...Exec` nodes) that
//! says *how* a query runs against storage, where the logical plan says only
//! *what*. Lowering turns a `LogicalPlan` into this; the executor walks it,
//! driving column-major batches up the tree.
//!
//! `Display` renders the conventional indented operator tree (outermost first):
//!
//! ```text
//! ProjectionExec [id, name]
//!   LimitExec 10
//!     SortExec [name]
//!       FilterExec [age > 18]
//!         TableScanExec users
//! ```

use std::fmt;

use crate::parser::ast::{ColumnDef, CompareOp, DataType, Expr, Literal, LogicalOp};
use crate::storage::{ScanCompare, ScanPredicate};

/// A node in the physical plan. `CreateTableExec` and `InsertExec` are
/// standalone roots that produce an effect, not rows. The relational operators
/// (`TableScanExec`, `FilterExec`, `ProjectionExec`, `SortExec`, `LimitExec`)
/// nest innermost → outermost with `TableScanExec` at the leaf, and each
/// produces a stream of column-major batches.
///
/// The leaf payloads — `ColumnDef`, `Literal`, and the `Expr` of a filter —
/// are the parser's AST types, reused here exactly as the logical plan reuses
/// them: they are stable, semantic value types with no behavior to re-model.
// The `…Exec` suffix is the conventional name for a physical operator and is
// shared by every variant on purpose, so the variant-name lint does not apply.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, PartialEq)]
pub enum PhysicalPlan {
    CreateTableExec {
        table: String,
        columns: Vec<ColumnDef>,
    },
    InsertExec {
        table: String,
        values: Vec<Literal>,
    },
    DropTableExec {
        table: String,
    },
    ShowTablesExec,
    DescribeExec {
        table: String,
        /// Also emit the table's storage-level metadata (id, storage track,
        /// row-group size) after the column rows.
        extended: bool,
    },
    TableScanExec {
        table: String,
        /// Columns to decode from storage, in output order. `None` means every
        /// column in schema order.
        projection: Option<Vec<String>>,
        /// Zone-map hints: `column <op> constant` conjuncts the scan uses to
        /// skip whole row groups via their INT `min`/`max` stats. Pruning is a
        /// pure optimization, so this may hold any conjunctive subset — or be
        /// empty.
        prune: Vec<ScanPredicate>,
    },
    FilterExec {
        predicate: Expr,
        input: Box<PhysicalPlan>,
    },
    ProjectionExec {
        columns: Vec<String>,
        input: Box<PhysicalPlan>,
    },
    SortExec {
        column: String,
        descending: bool,
        input: Box<PhysicalPlan>,
    },
    LimitExec {
        count: u64,
        input: Box<PhysicalPlan>,
    },
    /// Order by a column and keep only the first `count` rows — the fused form
    /// of a `SortExec` directly under a `LimitExec`. Produced by lowering the
    /// optimizer's `TopK`; runs as a bounded heap rather than a full sort.
    TopKExec {
        column: String,
        descending: bool,
        count: u64,
        input: Box<PhysicalPlan>,
    },
}

impl fmt::Display for PhysicalPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_indented(f, 0)
    }
}

impl PhysicalPlan {
    /// Write this node's header line at `depth` (two spaces per level), then
    /// recurse into the child. No trailing newline, so the caller's `println!`
    /// supplies exactly one.
    fn fmt_indented(&self, f: &mut fmt::Formatter<'_>, depth: usize) -> fmt::Result {
        let pad = "  ".repeat(depth);
        match self {
            PhysicalPlan::CreateTableExec { table, columns } => {
                let cols = columns
                    .iter()
                    .map(render_column_def)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "{pad}CreateTableExec {table} [{cols}]")
            }
            PhysicalPlan::InsertExec { table, values } => {
                let vals = values
                    .iter()
                    .map(render_literal)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "{pad}InsertExec {table} [{vals}]")
            }
            PhysicalPlan::DropTableExec { table } => write!(f, "{pad}DropTableExec {table}"),
            PhysicalPlan::ShowTablesExec => write!(f, "{pad}ShowTablesExec"),
            PhysicalPlan::DescribeExec { table, extended } => {
                let suffix = if *extended { " EXTENDED" } else { "" };
                write!(f, "{pad}DescribeExec {table}{suffix}")
            }
            PhysicalPlan::TableScanExec {
                table,
                projection,
                prune,
            } => {
                write!(f, "{pad}TableScanExec {table}")?;
                if let Some(cols) = projection {
                    write!(f, " [{}]", cols.join(", "))?;
                }
                if !prune.is_empty() {
                    let hints = prune
                        .iter()
                        .map(render_scan_predicate)
                        .collect::<Vec<_>>()
                        .join(", ");
                    write!(f, " prune=[{hints}]")?;
                }
                Ok(())
            }
            PhysicalPlan::FilterExec { predicate, input } => {
                writeln!(f, "{pad}FilterExec [{}]", render_expr(predicate))?;
                input.fmt_indented(f, depth + 1)
            }
            PhysicalPlan::ProjectionExec { columns, input } => {
                writeln!(f, "{pad}ProjectionExec [{}]", columns.join(", "))?;
                input.fmt_indented(f, depth + 1)
            }
            PhysicalPlan::SortExec {
                column,
                descending,
                input,
            } => {
                let dir = if *descending { " DESC" } else { "" };
                writeln!(f, "{pad}SortExec [{column}{dir}]")?;
                input.fmt_indented(f, depth + 1)
            }
            PhysicalPlan::LimitExec { count, input } => {
                writeln!(f, "{pad}LimitExec {count}")?;
                input.fmt_indented(f, depth + 1)
            }
            PhysicalPlan::TopKExec {
                column,
                descending,
                count,
                input,
            } => {
                let dir = if *descending { " DESC" } else { "" };
                writeln!(f, "{pad}TopKExec [{column}{dir}] {count}")?;
                input.fmt_indented(f, depth + 1)
            }
        }
    }
}

fn render_column_def(col: &ColumnDef) -> String {
    let null = if col.nullable { "" } else { " NOT NULL" };
    let ty = match col.ty {
        DataType::Int => "INT",
        DataType::Text => "TEXT",
    };
    format!("{} {}{}", col.name, ty, null)
}

fn render_literal(lit: &Literal) -> String {
    match lit {
        Literal::Int(n) => n.to_string(),
        Literal::Text(s) => format!("'{s}'"),
        Literal::Null => "NULL".to_string(),
    }
}

/// Render a WHERE expression in readable infix form, e.g. `age > 18`.
fn render_expr(expr: &Expr) -> String {
    match expr {
        Expr::Column(name) => name.clone(),
        Expr::Literal(lit) => render_literal(lit),
        Expr::Compare { left, op, right } => format!(
            "{} {} {}",
            render_expr(left),
            compare_op_str(*op),
            render_expr(right)
        ),
        Expr::Logical { left, op, right } => format!(
            "{} {} {}",
            render_expr(left),
            match op {
                LogicalOp::And => "AND",
                LogicalOp::Or => "OR",
            },
            render_expr(right)
        ),
    }
}

fn render_scan_predicate(p: &ScanPredicate) -> String {
    format!("{} {} {}", p.column, scan_compare_str(p.op), p.value)
}

fn compare_op_str(op: CompareOp) -> &'static str {
    match op {
        CompareOp::Eq => "=",
        CompareOp::NotEq => "!=",
        CompareOp::Lt => "<",
        CompareOp::LtEq => "<=",
        CompareOp::Gt => ">",
        CompareOp::GtEq => ">=",
    }
}

fn scan_compare_str(op: ScanCompare) -> &'static str {
    match op {
        ScanCompare::Eq => "=",
        ScanCompare::NotEq => "!=",
        ScanCompare::Lt => "<",
        ScanCompare::LtEq => "<=",
        ScanCompare::Gt => ">",
        ScanCompare::GtEq => ">=",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan() -> PhysicalPlan {
        PhysicalPlan::TableScanExec {
            table: "users".into(),
            projection: None,
            prune: Vec::new(),
        }
    }

    #[test]
    fn renders_select_tree_outermost_first() {
        let plan = PhysicalPlan::ProjectionExec {
            columns: vec!["id".into(), "name".into()],
            input: Box::new(PhysicalPlan::LimitExec {
                count: 10,
                input: Box::new(PhysicalPlan::SortExec {
                    column: "name".into(),
                    descending: false,
                    input: Box::new(PhysicalPlan::FilterExec {
                        predicate: Expr::Compare {
                            left: Box::new(Expr::Column("age".into())),
                            op: CompareOp::Gt,
                            right: Box::new(Expr::Literal(Literal::Int(18))),
                        },
                        input: Box::new(scan()),
                    }),
                }),
            }),
        };
        assert_eq!(
            plan.to_string(),
            "ProjectionExec [id, name]\n  LimitExec 10\n    SortExec [name]\n      FilterExec [age > 18]\n        TableScanExec users"
        );
    }

    #[test]
    fn scan_renders_projection_and_prune_hints() {
        let plan = PhysicalPlan::TableScanExec {
            table: "users".into(),
            projection: Some(vec!["id".into(), "name".into()]),
            prune: vec![ScanPredicate {
                column: "age".into(),
                op: ScanCompare::Gt,
                value: 18,
            }],
        };
        assert_eq!(
            plan.to_string(),
            "TableScanExec users [id, name] prune=[age > 18]"
        );
    }

    #[test]
    fn top_k_renders_direction_and_count() {
        let plan = PhysicalPlan::TopKExec {
            column: "age".into(),
            descending: true,
            count: 5,
            input: Box::new(scan()),
        };
        assert_eq!(
            plan.to_string(),
            "TopKExec [age DESC] 5\n  TableScanExec users"
        );
    }

    #[test]
    fn sort_renders_descending() {
        let plan = PhysicalPlan::SortExec {
            column: "name".into(),
            descending: true,
            input: Box::new(scan()),
        };
        assert_eq!(
            plan.to_string(),
            "SortExec [name DESC]\n  TableScanExec users"
        );
    }

    #[test]
    fn create_table_lists_columns_with_types() {
        let plan = PhysicalPlan::CreateTableExec {
            table: "users".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    ty: DataType::Int,
                    nullable: false,
                },
                ColumnDef {
                    name: "name".into(),
                    ty: DataType::Text,
                    nullable: true,
                },
            ],
        };
        assert_eq!(
            plan.to_string(),
            "CreateTableExec users [id INT NOT NULL, name TEXT]"
        );
    }
}
