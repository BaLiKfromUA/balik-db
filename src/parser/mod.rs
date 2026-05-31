//! SQL front end: turn a query string into an internal [`ast::Statement`].
//!
//! This subsystem is intentionally isolated. It does not touch storage, the
//! catalog, or query execution — its only job is text → AST, with clear errors
//! for malformed or unsupported input. It never panics on user input: every
//! failure path returns a [`ParseError`].
//!
//! Implementation strategy is library-based: the third-party `sqlparser` crate
//! tokenizes and parses, and [`lower`] converts its tree into our own AST. The
//! dependency is confined to this module — the rest of the system sees only
//! `ast` types. Anything outside the supported subset is rejected during
//! lowering rather than passed through.
//!
//! Supported subset:
//! - `CREATE TABLE name (col INT|TEXT, ...)`
//! - `INSERT INTO name VALUES (int|'text'|NULL, ...)` — single row
//! - `SELECT */cols FROM name [WHERE cond] [ORDER BY col [ASC|DESC]] [LIMIT n]`
//!   where `cond` combines `= != < <= > >=` with `AND`/`OR` over columns and
//!   literals (parentheses allowed).

pub mod ast;
mod error;
mod lower;

pub use ast::Statement;
pub use error::ParseError;

use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

/// Parse a single SQL statement into the internal AST.
///
/// Returns a [`ParseError`] for empty input, multiple statements, syntax
/// errors, or any construct outside the supported subset.
pub fn parse(sql: &str) -> Result<Statement, ParseError> {
    let dialect = GenericDialect {};
    let mut statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| ParseError::new(e.to_string()))?;

    match statements.len() {
        0 => Err(ParseError::new("empty input: expected a SQL statement")),
        1 => lower::lower_statement(statements.pop().unwrap()),
        _ => Err(ParseError::new(
            "expected a single SQL statement per query",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::ast::*;
    use super::parse;

    fn col(s: &str) -> Expr {
        Expr::Column(s.to_string())
    }
    fn int(n: i64) -> Expr {
        Expr::Literal(Literal::Int(n))
    }

    // ---- CREATE TABLE ----

    #[test]
    fn create_table_basic() {
        let stmt = parse("CREATE TABLE users (id INT, name TEXT, age INT)").unwrap();
        let Statement::CreateTable(ct) = stmt else {
            panic!("expected CreateTable, got {stmt:?}");
        };
        assert_eq!(ct.table, "users");
        assert_eq!(
            ct.columns,
            vec![
                ColumnDef {
                    name: "id".into(),
                    ty: DataType::Int
                },
                ColumnDef {
                    name: "name".into(),
                    ty: DataType::Text
                },
                ColumnDef {
                    name: "age".into(),
                    ty: DataType::Int
                },
            ]
        );
    }

    #[test]
    fn create_table_empty_columns_is_rejected() {
        assert!(parse("CREATE TABLE users ()").is_err());
    }

    #[test]
    fn create_table_unknown_type_is_rejected() {
        let err = parse("CREATE TABLE t (x FLOAT)").unwrap_err();
        assert!(err.to_string().contains("FLOAT"), "{err}");
    }

    // ---- INSERT ----

    #[test]
    fn insert_values_basic() {
        let stmt = parse("INSERT INTO users VALUES (1, 'Alice', NULL)").unwrap();
        let Statement::Insert(ins) = stmt else {
            panic!("expected Insert, got {stmt:?}");
        };
        assert_eq!(ins.table, "users");
        assert_eq!(
            ins.values,
            vec![
                Literal::Int(1),
                Literal::Text("Alice".into()),
                Literal::Null
            ]
        );
    }

    #[test]
    fn insert_negative_integer() {
        let Statement::Insert(ins) = parse("INSERT INTO t VALUES (-5)").unwrap() else {
            panic!("expected Insert");
        };
        assert_eq!(ins.values, vec![Literal::Int(-5)]);
    }

    #[test]
    fn insert_without_values_is_rejected() {
        assert!(parse("INSERT INTO users VALUES").is_err());
        assert!(parse("INSERT INTO users VALUES ()").is_err());
    }

    #[test]
    fn insert_multi_row_is_rejected_as_unsupported() {
        let err = parse("INSERT INTO users VALUES (1, 'a'), (2, 'b')").unwrap_err();
        assert!(err.to_string().contains("multi-row"), "{err}");
    }

    // ---- SELECT ----

    #[test]
    fn select_star() {
        let Statement::Select(s) = parse("SELECT * FROM users").unwrap() else {
            panic!("expected Select");
        };
        assert_eq!(s.projections, Projection::All);
        assert_eq!(s.from, "users");
        assert!(s.filter.is_none());
        assert!(s.order_by.is_none());
        assert!(s.limit.is_none());
    }

    #[test]
    fn select_columns() {
        let Statement::Select(s) = parse("SELECT id, name FROM users").unwrap() else {
            panic!("expected Select");
        };
        assert_eq!(
            s.projections,
            Projection::Columns(vec!["id".into(), "name".into()])
        );
    }

    #[test]
    fn select_where_comparison() {
        let Statement::Select(s) = parse("SELECT id, name FROM users WHERE age > 18").unwrap()
        else {
            panic!("expected Select");
        };
        assert_eq!(
            s.filter,
            Some(Expr::Compare {
                left: Box::new(col("age")),
                op: CompareOp::Gt,
                right: Box::new(int(18)),
            })
        );
    }

    #[test]
    fn select_where_string_eq_and_limit() {
        let Statement::Select(s) =
            parse("SELECT id FROM users WHERE name = 'Alice' LIMIT 10").unwrap()
        else {
            panic!("expected Select");
        };
        assert_eq!(
            s.filter,
            Some(Expr::Compare {
                left: Box::new(col("name")),
                op: CompareOp::Eq,
                right: Box::new(Expr::Literal(Literal::Text("Alice".into()))),
            })
        );
        assert_eq!(s.limit, Some(10));
    }

    #[test]
    fn select_all_comparison_operators() {
        for (sql, op) in [
            ("=", CompareOp::Eq),
            ("!=", CompareOp::NotEq),
            ("<", CompareOp::Lt),
            ("<=", CompareOp::LtEq),
            (">", CompareOp::Gt),
            (">=", CompareOp::GtEq),
        ] {
            let q = format!("SELECT id FROM t WHERE a {sql} 1");
            let Statement::Select(s) = parse(&q).unwrap() else {
                panic!("expected Select for {q}");
            };
            match s.filter {
                Some(Expr::Compare { op: got, .. }) => assert_eq!(got, op, "{q}"),
                other => panic!("expected comparison for {q}, got {other:?}"),
            }
        }
    }

    #[test]
    fn select_and_or() {
        let Statement::Select(s) =
            parse("SELECT id FROM t WHERE a > 1 AND b < 2 OR c = 3").unwrap()
        else {
            panic!("expected Select");
        };
        // OR has lower precedence, so it is the root.
        match s.filter {
            Some(Expr::Logical {
                op: LogicalOp::Or, ..
            }) => {}
            other => panic!("expected OR at the root, got {other:?}"),
        }
    }

    #[test]
    fn select_parenthesized_where() {
        let Statement::Select(s) = parse("SELECT id FROM t WHERE (a = 1)").unwrap() else {
            panic!("expected Select");
        };
        assert_eq!(
            s.filter,
            Some(Expr::Compare {
                left: Box::new(col("a")),
                op: CompareOp::Eq,
                right: Box::new(int(1)),
            })
        );
    }

    #[test]
    fn select_order_by_directions() {
        let Statement::Select(s) = parse("SELECT id FROM t ORDER BY name").unwrap() else {
            panic!("expected Select");
        };
        assert_eq!(
            s.order_by,
            Some(OrderBy {
                column: "name".into(),
                descending: false
            })
        );

        let Statement::Select(s) = parse("SELECT id FROM t ORDER BY name DESC").unwrap() else {
            panic!("expected Select");
        };
        assert_eq!(s.order_by.unwrap().descending, true);
    }

    #[test]
    fn select_full_clause_chain() {
        let stmt =
            parse("SELECT id, name FROM users WHERE age > 18 ORDER BY name LIMIT 10").unwrap();
        let Statement::Select(s) = stmt else {
            panic!("expected Select");
        };
        assert_eq!(
            s.projections,
            Projection::Columns(vec!["id".into(), "name".into()])
        );
        assert!(s.filter.is_some());
        assert_eq!(s.order_by.unwrap().column, "name");
        assert_eq!(s.limit, Some(10));
    }

    // ---- malformed input (the spec's diagnostic cases): must be Err, never panic ----

    #[test]
    fn malformed_inputs_return_errors_without_panicking() {
        for q in [
            "SELEC id FROM users",         // misspelled keyword
            "SELECT FROM users",           // no projection
            "INSERT INTO users VALUES",    // no values list
            "CREATE TABLE users ()",       // no columns
            "SELECT id users",             // missing FROM (parses as `id AS users`, no FROM)
            "SELECT id FROM users WHERE",  // WHERE without expression
            "SELECT id FROM users LIMIT",  // LIMIT without a number
            "",                            // empty input
            "   ",                         // whitespace only
        ] {
            assert!(parse(q).is_err(), "expected error for {q:?}");
        }
    }

    // ---- unsupported (valid SQL, outside our subset): clear error, not a half AST ----

    #[test]
    fn unsupported_constructs_are_rejected() {
        for q in [
            "SELECT a FROM x JOIN y ON x.id = y.id",
            "SELECT count(*) FROM t GROUP BY a",
            "SELECT a FROM t1, t2",
            "SELECT a FROM (SELECT a FROM t) s",
            "WITH cte AS (SELECT 1) SELECT * FROM cte",
            "SELECT a FROM t UNION SELECT a FROM u",
        ] {
            let err = parse(q).unwrap_err();
            assert!(
                err.to_string().contains("unsupported"),
                "expected an `unsupported` error for {q:?}, got: {err}"
            );
        }
    }
}
