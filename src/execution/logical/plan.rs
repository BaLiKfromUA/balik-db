//! The logical plan: a tree of logical operators describing *what* a query
//! does, independent of how it is executed. A later execution stage walks this
//! tree against storage; here it is only built, validated, and rendered.
//!
//! `Display` renders the conventional indented operator tree (outermost first):
//!
//! ```text
//! Projection [id, name]
//!   Limit 10
//!     Filter [age > 18]
//!       Scan users
//! ```
//!
//! The same structure also serializes to JSON via `serde`.

use std::fmt;

use serde::Serialize;

use crate::parser::ast::{ColumnDef, CompareOp, DataType, Expr, Literal, LogicalOp};

/// A node in the logical plan. `CreateTable` and `Insert` are standalone roots;
/// the relational operators (`Scan`, `Filter`, `Projection`, `Sort`, `Limit`)
/// nest innermost → outermost, with `Scan` always at the leaf. `TopK` is never
/// produced by the binder — the optimizer introduces it by fusing an adjacent
/// `Sort` and `Limit`.
///
/// The leaf payloads — `ColumnDef`, `Literal`, and the `Expr` in a `Filter` —
/// are the parser's AST types, reused here on purpose. These are stable,
/// semantic value types with no behavior to re-model, so wrapping them in a
/// parallel set of planner types would add boilerplate and conversions without
/// buying anything. The structural work that distinguishes a plan from an AST
/// happens in the *shape* of the tree (a SELECT becomes a nested
/// `Scan → Filter → Sort → Limit → Projection`, not a copy of `Select`), not in
/// the leaves. If the planner later needs to attach resolved/typed information
/// to expressions, that is the point to introduce dedicated plan-side types.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum LogicalPlan {
    CreateTable {
        table: String,
        columns: Vec<ColumnDef>,
    },
    Insert {
        table: String,
        values: Vec<Literal>,
    },
    DropTable {
        table: String,
    },
    ShowTables,
    Describe {
        table: String,
        /// Also report the table's storage-level metadata (id, storage track,
        /// row-group size), not just its columns.
        extended: bool,
    },
    Scan {
        table: String,
        /// Columns the scan must produce. `None` means all of the table's
        /// columns (the shape the binder emits); `Some` is the pruned set a
        /// column-pushdown rewrite leaves behind.
        #[serde(skip_serializing_if = "Option::is_none")]
        columns: Option<Vec<String>>,
    },
    Filter {
        predicate: Expr,
        input: Box<LogicalPlan>,
    },
    Projection {
        columns: Vec<String>,
        input: Box<LogicalPlan>,
    },
    Sort {
        column: String,
        descending: bool,
        input: Box<LogicalPlan>,
    },
    Limit {
        count: u64,
        input: Box<LogicalPlan>,
    },
    /// Order by a column and keep only the first `count` rows — the fusion of a
    /// `Sort` directly under a `Limit`. Optimizer-only; the binder never emits
    /// this node.
    TopK {
        column: String,
        descending: bool,
        count: u64,
        input: Box<LogicalPlan>,
    },
}

impl fmt::Display for LogicalPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_indented(f, 0)
    }
}

impl LogicalPlan {
    /// Write this node's header line at `depth` (two spaces per level), then
    /// recurse into the child. No trailing newline, so the caller's `println!`
    /// supplies exactly one.
    fn fmt_indented(&self, f: &mut fmt::Formatter<'_>, depth: usize) -> fmt::Result {
        let pad = "  ".repeat(depth);
        match self {
            LogicalPlan::CreateTable { table, columns } => {
                let cols = columns
                    .iter()
                    .map(render_column_def)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "{pad}CreateTable {table} [{cols}]")
            }
            LogicalPlan::Insert { table, values } => {
                let vals = values
                    .iter()
                    .map(render_literal)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "{pad}Insert {table} [{vals}]")
            }
            LogicalPlan::DropTable { table } => write!(f, "{pad}DropTable {table}"),
            LogicalPlan::ShowTables => write!(f, "{pad}ShowTables"),
            LogicalPlan::Describe { table, extended } => {
                let suffix = if *extended { " EXTENDED" } else { "" };
                write!(f, "{pad}Describe {table}{suffix}")
            }
            LogicalPlan::Scan { table, columns } => match columns {
                Some(cols) => write!(f, "{pad}Scan {table} [{}]", cols.join(", ")),
                None => write!(f, "{pad}Scan {table}"),
            },
            LogicalPlan::Filter { predicate, input } => {
                writeln!(f, "{pad}Filter [{}]", render_expr(predicate))?;
                input.fmt_indented(f, depth + 1)
            }
            LogicalPlan::Projection { columns, input } => {
                writeln!(f, "{pad}Projection [{}]", columns.join(", "))?;
                input.fmt_indented(f, depth + 1)
            }
            LogicalPlan::Sort {
                column,
                descending,
                input,
            } => {
                let dir = if *descending { " DESC" } else { "" };
                writeln!(f, "{pad}Sort [{column}{dir}]")?;
                input.fmt_indented(f, depth + 1)
            }
            LogicalPlan::Limit { count, input } => {
                writeln!(f, "{pad}Limit {count}")?;
                input.fmt_indented(f, depth + 1)
            }
            LogicalPlan::TopK {
                column,
                descending,
                count,
                input,
            } => {
                let dir = if *descending { " DESC" } else { "" };
                writeln!(f, "{pad}TopK [{column}{dir}] {count}")?;
                input.fmt_indented(f, depth + 1)
            }
        }
    }
}

fn render_column_def(col: &ColumnDef) -> String {
    let null = if col.nullable { "" } else { " NOT NULL" };
    format!("{} {}{}", col.name, render_data_type(col.ty), null)
}

fn render_data_type(ty: DataType) -> &'static str {
    match ty {
        DataType::Int => "INT",
        DataType::Text => "TEXT",
    }
}

fn render_literal(lit: &Literal) -> String {
    match lit {
        Literal::Int(n) => n.to_string(),
        Literal::Text(s) => format!("'{s}'"),
        Literal::Null => "NULL".to_string(),
    }
}

/// Render a WHERE expression in readable infix form, e.g. `age > 18`,
/// `name = 'Alice'`, `a > 1 AND b < 2`.
fn render_expr(expr: &Expr) -> String {
    match expr {
        Expr::Column(name) => name.clone(),
        Expr::Literal(lit) => render_literal(lit),
        Expr::Compare { left, op, right } => {
            format!(
                "{} {} {}",
                render_expr(left),
                render_compare_op(*op),
                render_expr(right)
            )
        }
        Expr::Logical { left, op, right } => {
            format!(
                "{} {} {}",
                render_expr(left),
                render_logical_op(*op),
                render_expr(right)
            )
        }
    }
}

fn render_compare_op(op: CompareOp) -> &'static str {
    match op {
        CompareOp::Eq => "=",
        CompareOp::NotEq => "!=",
        CompareOp::Lt => "<",
        CompareOp::LtEq => "<=",
        CompareOp::Gt => ">",
        CompareOp::GtEq => ">=",
    }
}

fn render_logical_op(op: LogicalOp) -> &'static str {
    match op {
        LogicalOp::And => "AND",
        LogicalOp::Or => "OR",
    }
}

/// Append every column referenced by `expr` to `out`, left-to-right and with
/// duplicates, so callers can see exactly which columns a predicate touches.
/// Shared by the binder (to validate references) and the optimizer (to decide
/// which columns a scan must produce).
pub(crate) fn collect_expr_columns(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Column(name) => out.push(name.clone()),
        Expr::Literal(_) => {}
        Expr::Compare { left, right, .. } | Expr::Logical { left, right, .. } => {
            collect_expr_columns(left, out);
            collect_expr_columns(right, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan() -> LogicalPlan {
        LogicalPlan::Scan {
            table: "users".into(),
            columns: None,
        }
    }

    #[test]
    fn renders_select_tree_outermost_first() {
        let plan = LogicalPlan::Projection {
            columns: vec!["id".into(), "name".into()],
            input: Box::new(LogicalPlan::Limit {
                count: 10,
                input: Box::new(LogicalPlan::Filter {
                    predicate: Expr::Compare {
                        left: Box::new(Expr::Column("age".into())),
                        op: CompareOp::Gt,
                        right: Box::new(Expr::Literal(Literal::Int(18))),
                    },
                    input: Box::new(scan()),
                }),
            }),
        };
        assert_eq!(
            plan.to_string(),
            "Projection [id, name]\n  Limit 10\n    Filter [age > 18]\n      Scan users"
        );
    }

    #[test]
    fn renders_text_literal_quoted_in_filter() {
        let plan = LogicalPlan::Filter {
            predicate: Expr::Compare {
                left: Box::new(Expr::Column("name".into())),
                op: CompareOp::Eq,
                right: Box::new(Expr::Literal(Literal::Text("Alice".into()))),
            },
            input: Box::new(scan()),
        };
        assert_eq!(plan.to_string(), "Filter [name = 'Alice']\n  Scan users");
    }

    #[test]
    fn renders_sort_direction() {
        let plan = LogicalPlan::Sort {
            column: "name".into(),
            descending: true,
            input: Box::new(scan()),
        };
        assert_eq!(plan.to_string(), "Sort [name DESC]\n  Scan users");
    }

    #[test]
    fn create_table_lists_columns_with_types() {
        let plan = LogicalPlan::CreateTable {
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
            "CreateTable users [id INT NOT NULL, name TEXT]"
        );
    }

    #[test]
    fn serializes_to_json() {
        let json = serde_json::to_string(&scan()).unwrap();
        assert_eq!(json, r#"{"Scan":{"table":"users"}}"#);
    }

    #[test]
    fn renders_top_k_with_direction_and_count() {
        let plan = LogicalPlan::TopK {
            column: "age".into(),
            descending: true,
            count: 5,
            input: Box::new(scan()),
        };
        assert_eq!(plan.to_string(), "TopK [age DESC] 5\n  Scan users");
    }

    #[test]
    fn scan_with_columns_renders_and_serializes() {
        let plan = LogicalPlan::Scan {
            table: "users".into(),
            columns: Some(vec!["id".into(), "name".into()]),
        };
        assert_eq!(plan.to_string(), "Scan users [id, name]");
        assert_eq!(
            serde_json::to_string(&plan).unwrap(),
            r#"{"Scan":{"table":"users","columns":["id","name"]}}"#
        );
    }
}
