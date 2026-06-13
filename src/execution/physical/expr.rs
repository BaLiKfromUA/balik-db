//! Vectorized predicate evaluation over a column-major batch.
//!
//! Evaluation is column-at-a-time: each expression node produces a whole vector
//! rather than being re-walked per row. A comparison turns its two operands into
//! value vectors and compares them element-wise into a boolean *selection mask*;
//! a logical node combines two masks. `FilterExec` then uses that mask to
//! compact the batch.
//!
//! NULL handling is the minimal two-valued form: any comparison touching NULL is
//! false, and `AND`/`OR` treat that as a plain `false`. A comparison between
//! mismatched types (e.g. text vs. int, which the binder does not currently
//! reject) also yields false rather than erroring.

use std::cmp::Ordering;

use crate::error::Error;
use crate::parser::ast::{CompareOp, Expr, Literal, LogicalOp};
use crate::storage::{ColumnBatch, Value};

/// Evaluate boolean `predicate` against `batch`, returning a selection mask with
/// one entry per row: `true` keeps the row.
pub(super) fn evaluate_predicate(
    predicate: &Expr,
    batch: &ColumnBatch,
) -> Result<Vec<bool>, Error> {
    eval_mask(predicate, batch)
}

/// One side of a comparison, resolved against the batch: either a real column
/// vector or a single literal broadcast across every row.
enum Operand<'a> {
    Column(&'a [Value]),
    Scalar(Value),
}

impl Operand<'_> {
    fn value_at(&self, row: usize) -> &Value {
        match self {
            Operand::Column(col) => &col[row],
            Operand::Scalar(v) => v,
        }
    }
}

fn eval_mask(expr: &Expr, batch: &ColumnBatch) -> Result<Vec<bool>, Error> {
    let rows = batch.num_rows();
    match expr {
        Expr::Compare { left, op, right } => {
            let lhs = operand(left, batch)?;
            let rhs = operand(right, batch)?;
            Ok((0..rows)
                .map(|r| compare(lhs.value_at(r), *op, rhs.value_at(r)))
                .collect())
        }
        Expr::Logical { left, op, right } => {
            let lhs = eval_mask(left, batch)?;
            let rhs = eval_mask(right, batch)?;
            Ok((0..rows)
                .map(|r| match op {
                    LogicalOp::And => lhs[r] && rhs[r],
                    LogicalOp::Or => lhs[r] || rhs[r],
                })
                .collect())
        }
        Expr::Column(_) | Expr::Literal(_) => Err(Error::other(
            "filter predicate must be a boolean expression, not a bare column or literal",
        )),
    }
}

/// Resolve a comparison operand to a column vector or a broadcast scalar.
fn operand<'a>(expr: &Expr, batch: &'a ColumnBatch) -> Result<Operand<'a>, Error> {
    match expr {
        Expr::Column(name) => {
            let idx = batch.names.iter().position(|n| n == name).ok_or_else(|| {
                Error::other(format!(
                    "filter references column '{name}' not present in the scanned batch"
                ))
            })?;
            Ok(Operand::Column(&batch.columns[idx]))
        }
        Expr::Literal(lit) => Ok(Operand::Scalar(literal_value(lit))),
        _ => Err(Error::other(
            "a comparison operand must be a column or a literal",
        )),
    }
}

fn literal_value(lit: &Literal) -> Value {
    match lit {
        Literal::Int(n) => Value::Int(*n),
        Literal::Text(s) => Value::Text(s.clone()),
        Literal::Null => Value::Null,
    }
}

/// Compare two values under `op`. NULL on either side, or a type mismatch, is
/// never a match.
fn compare(a: &Value, op: CompareOp, b: &Value) -> bool {
    let ord = match (a, b) {
        (Value::Null, _) | (_, Value::Null) => return false,
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        _ => return false,
    };
    match op {
        CompareOp::Eq => ord == Ordering::Equal,
        CompareOp::NotEq => ord != Ordering::Equal,
        CompareOp::Lt => ord == Ordering::Less,
        CompareOp::LtEq => ord != Ordering::Greater,
        CompareOp::Gt => ord == Ordering::Greater,
        CompareOp::GtEq => ord != Ordering::Less,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch() -> ColumnBatch {
        ColumnBatch {
            names: vec!["id".into(), "name".into()],
            columns: vec![
                vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Null],
                vec![
                    Value::Text("a".into()),
                    Value::Text("b".into()),
                    Value::Text("c".into()),
                    Value::Text("d".into()),
                ],
            ],
        }
    }

    fn col(name: &str) -> Box<Expr> {
        Box::new(Expr::Column(name.into()))
    }

    fn int(n: i64) -> Box<Expr> {
        Box::new(Expr::Literal(Literal::Int(n)))
    }

    fn cmp(left: Box<Expr>, op: CompareOp, right: Box<Expr>) -> Expr {
        Expr::Compare { left, op, right }
    }

    #[test]
    fn greater_than_on_int_column() {
        let mask = evaluate_predicate(&cmp(col("id"), CompareOp::Gt, int(1)), &batch()).unwrap();
        assert_eq!(mask, vec![false, true, true, false]);
    }

    #[test]
    fn all_six_comparisons() {
        let b = batch();
        let eval = |op| evaluate_predicate(&cmp(col("id"), op, int(2)), &b).unwrap();
        assert_eq!(eval(CompareOp::Eq), vec![false, true, false, false]);
        assert_eq!(eval(CompareOp::NotEq), vec![true, false, true, false]);
        assert_eq!(eval(CompareOp::Lt), vec![true, false, false, false]);
        assert_eq!(eval(CompareOp::LtEq), vec![true, true, false, false]);
        assert_eq!(eval(CompareOp::Gt), vec![false, false, true, false]);
        assert_eq!(eval(CompareOp::GtEq), vec![false, true, true, false]);
    }

    #[test]
    fn null_never_matches() {
        // The 4th id is NULL: false for every operator, including `!=`.
        let mask =
            evaluate_predicate(&cmp(col("id"), CompareOp::NotEq, int(999)), &batch()).unwrap();
        assert_eq!(mask, vec![true, true, true, false]);
    }

    #[test]
    fn text_comparison_is_lexicographic() {
        let pred = cmp(
            col("name"),
            CompareOp::GtEq,
            Box::new(Expr::Literal(Literal::Text("c".into()))),
        );
        let mask = evaluate_predicate(&pred, &batch()).unwrap();
        assert_eq!(mask, vec![false, false, true, true]);
    }

    #[test]
    fn and_combines_masks() {
        let pred = Expr::Logical {
            left: Box::new(cmp(col("id"), CompareOp::GtEq, int(2))),
            op: LogicalOp::And,
            right: Box::new(cmp(col("id"), CompareOp::LtEq, int(3))),
        };
        let mask = evaluate_predicate(&pred, &batch()).unwrap();
        assert_eq!(mask, vec![false, true, true, false]);
    }

    #[test]
    fn or_combines_masks() {
        let pred = Expr::Logical {
            left: Box::new(cmp(col("id"), CompareOp::Eq, int(1))),
            op: LogicalOp::Or,
            right: Box::new(cmp(col("id"), CompareOp::Eq, int(3))),
        };
        let mask = evaluate_predicate(&pred, &batch()).unwrap();
        assert_eq!(mask, vec![true, false, true, false]);
    }

    #[test]
    fn literal_on_the_left_is_evaluated_directly() {
        // 2 < id
        let mask = evaluate_predicate(&cmp(int(2), CompareOp::Lt, col("id")), &batch()).unwrap();
        assert_eq!(mask, vec![false, false, true, false]);
    }

    #[test]
    fn unknown_column_errors() {
        let pred = cmp(col("ghost"), CompareOp::Eq, int(1));
        assert!(evaluate_predicate(&pred, &batch()).is_err());
    }
}
