//! Extracting row-group pruning hints from a WHERE predicate.
//!
//! A scan can skip a whole row group when an INT column's `min`/`max` zone map
//! proves no row in it can satisfy the query. To feed that, we pull the
//! `column <op> integer` comparisons out of a filter expression as
//! [`ScanPredicate`]s.
//!
//! Only *mandatory* comparisons are extracted: we descend through `AND` (every
//! conjunct must hold, so each is a valid skip hint) but stop at `OR` (a row can
//! satisfy the predicate through either branch, so neither branch alone bounds
//! the group). Non-INT and non-literal comparisons are ignored. Pruning is a
//! pure optimization, so dropping anything we cannot use only forgoes a skip —
//! it never changes results.

use crate::parser::ast::{CompareOp, Expr, Literal};
use crate::storage::{ScanCompare, ScanPredicate};

/// Collect the conjunctive `column <op> integer` comparisons of `expr` as
/// pruning hints. May return fewer than appear in the expression.
pub(super) fn extract_scan_predicates(expr: &Expr) -> Vec<ScanPredicate> {
    let mut out = Vec::new();
    collect(expr, &mut out);
    out
}

fn collect(expr: &Expr, out: &mut Vec<ScanPredicate>) {
    match expr {
        Expr::Logical {
            left,
            op: crate::parser::ast::LogicalOp::And,
            right,
        } => {
            collect(left, out);
            collect(right, out);
        }
        Expr::Compare { left, op, right } => {
            if let Some(p) = as_scan_predicate(left, *op, right) {
                out.push(p);
            }
        }
        // `OR` cannot yield a mandatory bound; columns and bare literals are not
        // comparisons. Nothing to extract.
        _ => {}
    }
}

/// Turn `left <op> right` into a pruning hint when one side is a column and the
/// other an INT literal. When the literal is on the left, the operator is
/// flipped so the hint always reads `column <op> value`.
fn as_scan_predicate(left: &Expr, op: CompareOp, right: &Expr) -> Option<ScanPredicate> {
    match (left, right) {
        (Expr::Column(column), Expr::Literal(Literal::Int(value))) => Some(ScanPredicate {
            column: column.clone(),
            op: scan_compare(op),
            value: *value,
        }),
        (Expr::Literal(Literal::Int(value)), Expr::Column(column)) => Some(ScanPredicate {
            column: column.clone(),
            op: scan_compare(flip(op)),
            value: *value,
        }),
        _ => None,
    }
}

fn scan_compare(op: CompareOp) -> ScanCompare {
    match op {
        CompareOp::Eq => ScanCompare::Eq,
        CompareOp::NotEq => ScanCompare::NotEq,
        CompareOp::Lt => ScanCompare::Lt,
        CompareOp::LtEq => ScanCompare::LtEq,
        CompareOp::Gt => ScanCompare::Gt,
        CompareOp::GtEq => ScanCompare::GtEq,
    }
}

/// Reverse an operator's sense for swapped operands: `5 < age` is `age > 5`.
fn flip(op: CompareOp) -> CompareOp {
    match op {
        CompareOp::Eq => CompareOp::Eq,
        CompareOp::NotEq => CompareOp::NotEq,
        CompareOp::Lt => CompareOp::Gt,
        CompareOp::LtEq => CompareOp::GtEq,
        CompareOp::Gt => CompareOp::Lt,
        CompareOp::GtEq => CompareOp::LtEq,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ast::LogicalOp;

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
    fn extracts_column_op_literal() {
        let preds = extract_scan_predicates(&cmp(col("age"), CompareOp::Gt, int(18)));
        assert_eq!(
            preds,
            vec![ScanPredicate {
                column: "age".into(),
                op: ScanCompare::Gt,
                value: 18,
            }]
        );
    }

    #[test]
    fn flips_operator_when_literal_is_on_the_left() {
        let preds = extract_scan_predicates(&cmp(int(18), CompareOp::Lt, col("age")));
        assert_eq!(
            preds,
            vec![ScanPredicate {
                column: "age".into(),
                op: ScanCompare::Gt,
                value: 18,
            }]
        );
    }

    #[test]
    fn descends_through_and_collecting_both_sides() {
        let expr = Expr::Logical {
            left: Box::new(cmp(col("age"), CompareOp::GtEq, int(18))),
            op: LogicalOp::And,
            right: Box::new(cmp(col("id"), CompareOp::Lt, int(100))),
        };
        let preds = extract_scan_predicates(&expr);
        assert_eq!(preds.len(), 2);
        assert_eq!(preds[0].column, "age");
        assert_eq!(preds[1].column, "id");
    }

    #[test]
    fn ignores_predicates_under_or() {
        let expr = Expr::Logical {
            left: Box::new(cmp(col("age"), CompareOp::Gt, int(18))),
            op: LogicalOp::Or,
            right: Box::new(cmp(col("id"), CompareOp::Lt, int(100))),
        };
        assert!(extract_scan_predicates(&expr).is_empty());
    }

    #[test]
    fn ignores_non_int_literal_comparisons() {
        let expr = cmp(
            col("name"),
            CompareOp::Eq,
            Box::new(Expr::Literal(Literal::Text("Alice".into()))),
        );
        assert!(extract_scan_predicates(&expr).is_empty());
    }

    #[test]
    fn keeps_and_conjuncts_but_drops_nested_or() {
        // age > 1 AND (id < 2 OR id > 9)  →  only `age > 1` is mandatory.
        let nested_or = Expr::Logical {
            left: Box::new(cmp(col("id"), CompareOp::Lt, int(2))),
            op: LogicalOp::Or,
            right: Box::new(cmp(col("id"), CompareOp::Gt, int(9))),
        };
        let expr = Expr::Logical {
            left: Box::new(cmp(col("age"), CompareOp::Gt, int(1))),
            op: LogicalOp::And,
            right: Box::new(nested_or),
        };
        let preds = extract_scan_predicates(&expr);
        assert_eq!(preds.len(), 1);
        assert_eq!(preds[0].column, "age");
    }
}
